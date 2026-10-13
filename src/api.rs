use std::{
    collections::BTreeMap,
    io::SeekFrom,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            ACCEPT, ACCEPT_RANGES, AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION,
            CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE,
            VARY, X_CONTENT_TYPE_OPTIONS,
        },
    },
    response::Response,
};
use futures_util::StreamExt;
use serde::Serialize;
use subtle::ConstantTimeEq;
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    error::ApiError,
    ident::AssetId,
    media::{MediaError, sha256_file, validate_signature},
    model::{AssetRecord, RenditionFormat, RenditionRecord, SourceFormat, StoredAsset},
    store::InsertOutcome,
};

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";
const MAX_ID_ATTEMPTS: usize = 5;

#[derive(Serialize)]
pub(super) struct HealthResponse {
    status: &'static str,
}

pub(crate) async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
pub(super) struct AssetResponse {
    id: String,
    created_at: i64,
    original: OriginalResponse,
    renditions: Vec<RenditionResponse>,
}

#[derive(Serialize)]
pub(super) struct OriginalResponse {
    url: String,
    content_type: &'static str,
    bytes: u64,
    sha256: String,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Serialize)]
pub(super) struct RenditionResponse {
    max_width: u32,
    width: u32,
    height: u32,
    url: String,
    formats: Vec<&'static str>,
}

impl From<&StoredAsset> for AssetResponse {
    fn from(value: &StoredAsset) -> Self {
        let asset = &value.asset;
        let mut grouped = BTreeMap::<u32, (u32, u32, Vec<&'static str>)>::new();
        for rendition in &value.renditions {
            let entry = grouped
                .entry(rendition.max_width)
                .or_insert_with(|| (rendition.width, rendition.height, Vec::new()));
            entry.2.push(rendition.format.key());
        }

        let renditions = grouped
            .into_iter()
            .map(|(max_width, (width, height, mut formats))| {
                formats.sort_unstable();
                RenditionResponse {
                    max_width,
                    width,
                    height,
                    url: format!("/media/{}/w{max_width}", asset.id),
                    formats,
                }
            })
            .collect();

        Self {
            id: asset.id.to_string(),
            created_at: asset.created_at,
            original: OriginalResponse {
                url: format!("/media/{}/original", asset.id),
                content_type: asset.source_format.mime_type(),
                bytes: asset.bytes,
                sha256: asset.sha256.clone(),
                width: asset.width,
                height: asset.height,
            },
            renditions,
        }
    }
}

pub(super) async fn upload_asset(
    State(state): State<AppState>,
    request: Request,
) -> Result<(StatusCode, Json<AssetResponse>), ApiError> {
    authorize(&state, request.headers())?;
    let headers = request.headers();
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::unsupported("Set a supported Content-Type for the file."))?;
    let source_format = SourceFormat::parse_content_type(content_type)
        .ok_or_else(|| ApiError::unsupported("This file type is not accepted."))?;
    let limit = if source_format.is_image() {
        state.config.max_image_bytes
    } else {
        state.config.max_document_bytes
    };

    if let Some(encoding) = headers.get("content-encoding") {
        let identity = encoding
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("identity"));
        if !identity {
            return Err(ApiError::unsupported(
                "Compressed request bodies are not supported.",
            ));
        }
    }
    if let Some(content_length) = headers.get(CONTENT_LENGTH) {
        let declared = content_length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                ApiError::bad_request("invalid_content_length", "Invalid Content-Length.")
            })?;
        if declared > limit {
            return Err(ApiError::too_large());
        }
    }

    let _upload_permit = state
        .upload_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::unavailable("Atlas is currently at upload capacity."))?;
    let id = AssetId::generate(state.config.id_length).map_err(ApiError::internal)?;
    let staging_dir = state.staging_dir(&id);
    fs::create_dir(&staging_dir)
        .await
        .map_err(ApiError::internal)?;
    let result = create_asset(&state, id, source_format, request.into_body(), &staging_dir).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_dir).await;
    }
    result
}

async fn create_asset(
    state: &AppState,
    id: AssetId,
    source_format: SourceFormat,
    body: Body,
    staging_dir: &std::path::Path,
) -> Result<(StatusCode, Json<AssetResponse>), ApiError> {
    let upload_path = staging_dir.join("upload.part");
    let limit = if source_format.is_image() {
        state.config.max_image_bytes
    } else {
        state.config.max_document_bytes
    };
    let (bytes, prefix) = stream_upload(body, &upload_path, limit).await?;
    if !validate_signature(source_format, &prefix) {
        return Err(ApiError::unsupported(
            "The file contents do not match the declared media type, or the format is not allowed.",
        ));
    }

    let original_path = staging_dir.join(format!("original.{}", source_format.extension()));
    fs::rename(&upload_path, &original_path)
        .await
        .map_err(ApiError::internal)?;
    let original_sha256 = sha256_file(&original_path)
        .await
        .map_err(ApiError::internal)?;

    let (dimensions, renditions) = if source_format.is_image() {
        let _permit = state
            .conversion_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(ApiError::internal)?;
        let processed = tokio::time::timeout(state.config.image_processing_timeout, async {
            let dimensions = state
                .image_processor
                .inspect(&original_path, source_format, staging_dir)
                .await?;
            let renditions = state
                .image_processor
                .generate(
                    &original_path,
                    source_format,
                    dimensions,
                    &state.config.variant_widths,
                    staging_dir,
                )
                .await?;
            Ok::<_, MediaError>((dimensions, renditions))
        })
        .await
        .map_err(|_| ApiError::invalid("Image processing exceeded its time limit."))?
        .map_err(map_media_error)?;
        let (dimensions, renditions) = processed;
        (Some(dimensions), renditions)
    } else {
        (None, Vec::new())
    };

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(ApiError::internal)?
        .as_secs() as i64;
    let mut record = AssetRecord {
        id,
        source_format,
        bytes,
        sha256: original_sha256,
        width: dimensions.map(|value| value.width),
        height: dimensions.map(|value| value.height),
        created_at,
    };

    let mut final_dir = state.asset_dir(&record.id);
    fs::rename(staging_dir, &final_dir)
        .await
        .map_err(ApiError::internal)?;

    let mut attempts = 0_usize;

    // The bytes on disk are already final. If this ID lost a race, generate a
    // new one and rename the directory rather than reprocessing the upload.
    let stored = loop {
        match state.store.insert_asset(&record, &renditions).await {
            Ok(InsertOutcome::Created) => {
                break StoredAsset {
                    asset: record,
                    renditions,
                };
            }
            Ok(InsertOutcome::DuplicateId) if attempts < MAX_ID_ATTEMPTS => {
                attempts += 1;
                let next = AssetId::generate(state.config.id_length).map_err(ApiError::internal)?;
                let next_dir = state.asset_dir(&next);
                fs::rename(&final_dir, &next_dir)
                    .await
                    .map_err(ApiError::internal)?;
                record.id = next;
                final_dir = next_dir;
            }
            Ok(InsertOutcome::DuplicateId) => {
                let _ = fs::remove_dir_all(&final_dir).await;
                return Err(ApiError::internal(
                    "could not allocate a unique asset ID after repeated collisions",
                ));
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&final_dir).await;
                return Err(ApiError::internal(error));
            }
        }
    };

    Ok((StatusCode::CREATED, Json(AssetResponse::from(&stored))))
}

async fn stream_upload(
    body: Body,
    path: &std::path::Path,
    limit: u64,
) -> Result<(u64, Vec<u8>), ApiError> {
    let mut stream = body.into_data_stream();
    let mut file = File::create(path).await.map_err(ApiError::internal)?;
    let mut total = 0_u64;
    let mut prefix = Vec::with_capacity(1024);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            ApiError::bad_request(
                "invalid_request_body",
                "The request body could not be read.",
            )
        })?;
        total = total.saturating_add(chunk.len() as u64);
        if total > limit {
            return Err(ApiError::too_large());
        }
        if prefix.len() < 1024 {
            let take = (1024 - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
        }
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
    }

    if total == 0 {
        return Err(ApiError::bad_request(
            "empty_upload",
            "The uploaded file is empty.",
        ));
    }
    file.sync_all().await.map_err(ApiError::internal)?;
    Ok((total, prefix))
}

pub(super) async fn get_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AssetResponse>, ApiError> {
    authorize(&state, &headers)?;
    let id = parse_id(&id)?;
    let asset = state
        .store
        .get_asset(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(AssetResponse::from(&asset)))
}

pub(super) async fn get_original(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let asset = state
        .store
        .get_asset(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;
    let path = state.asset_dir(&id).join(format!(
        "original.{}",
        asset.asset.source_format.extension()
    ));
    let disposition = if matches!(
        asset.asset.source_format,
        SourceFormat::Doc | SourceFormat::Docx
    ) {
        "attachment"
    } else {
        "inline"
    };
    serve_file(
        &headers,
        &path,
        asset.asset.bytes,
        &asset.asset.sha256,
        asset.asset.source_format.mime_type(),
        disposition,
        false,
    )
    .await
}

pub(super) async fn get_rendition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let max_width = name
        .strip_prefix('w')
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(ApiError::not_found)?;
    let asset = state
        .store
        .get_asset(&id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;
    let available: Vec<_> = asset
        .renditions
        .iter()
        .filter(|rendition| rendition.max_width == max_width)
        .collect();
    if available.is_empty() {
        return Err(ApiError::not_found());
    }
    let selected = select_format(&headers, &available).ok_or_else(ApiError::not_acceptable)?;
    let path = state
        .asset_dir(&id)
        .join(format!("w{max_width}.{}", selected.format.extension()));
    serve_file(
        &headers,
        &path,
        selected.bytes,
        &selected.sha256,
        selected.format.mime_type(),
        "inline",
        true,
    )
    .await
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Err(ApiError::unauthorized());
    };
    let Ok(value) = value.to_str() else {
        return Err(ApiError::unauthorized());
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return Err(ApiError::unauthorized());
    };
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() || token.contains(' ') {
        return Err(ApiError::unauthorized());
    }
    if state
        .config
        .api_token
        .as_bytes()
        .ct_eq(token.as_bytes())
        .unwrap_u8()
        == 1
    {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn parse_id(value: &str) -> Result<AssetId, ApiError> {
    AssetId::parse(value).map_err(|_| ApiError::not_found())
}

fn map_media_error(error: MediaError) -> ApiError {
    match error {
        MediaError::Invalid(detail) => ApiError::invalid(detail),
        MediaError::LimitExceeded => ApiError::too_large(),
        MediaError::TimedOut => ApiError::invalid("Image processing exceeded its time limit."),
        MediaError::ProcessUnavailable(error) => {
            tracing::error!(%error, "could not start ImageMagick");
            ApiError::unavailable("The image processor is not available.")
        }
        MediaError::ProcessFailed => ApiError::invalid("The image could not be processed."),
        MediaError::Io(error) => ApiError::internal(error),
    }
}

fn select_format<'a>(
    headers: &HeaderMap,
    choices: &[&'a RenditionRecord],
) -> Option<&'a RenditionRecord> {
    let accept = headers.get(ACCEPT).and_then(|value| value.to_str().ok());
    let Some(accept) = accept else {
        return choices
            .iter()
            .copied()
            .find(|choice| is_fallback(choice.format));
    };

    let preferred = [
        RenditionFormat::Jxl,
        RenditionFormat::Avif,
        RenditionFormat::Webp,
    ];
    let mut selected: Option<(f32, usize, &RenditionRecord)> = None;
    for (rank, format) in preferred.into_iter().enumerate() {
        let Some(choice) = choices
            .iter()
            .copied()
            .find(|choice| choice.format == format)
        else {
            continue;
        };
        if let Some(quality) = accepted_quality(accept, format.mime_type())
            && quality > 0.0
            && selected.is_none_or(|(best_quality, best_rank, _)| {
                quality > best_quality || (quality == best_quality && rank < best_rank)
            })
        {
            selected = Some((quality, rank, choice));
        }
    }
    if let Some((_, _, choice)) = selected {
        return Some(choice);
    }

    choices.iter().copied().find(|choice| {
        is_fallback(choice.format)
            && accepted_quality_with_wildcards(accept, choice.format.mime_type())
                .is_some_and(|quality| quality > 0.0)
    })
}

fn accepted_quality(accept: &str, mime_type: &str) -> Option<f32> {
    let mut best = None;
    for entry in accept.split(',') {
        let mut parts = entry.trim().split(';');
        let Some(media_type) = parts.next().map(str::trim) else {
            continue;
        };
        if media_type.eq_ignore_ascii_case(mime_type) {
            let quality = parse_quality(parts);
            best = Some(best.unwrap_or(0.0_f32).max(quality));
        }
    }
    best
}

fn accepted_quality_with_wildcards(accept: &str, mime_type: &str) -> Option<f32> {
    if let Some(exact) = accepted_quality(accept, mime_type) {
        return Some(exact);
    }
    let mut image_wildcard = None;
    let mut global_wildcard = None;
    for entry in accept.split(',') {
        let mut parts = entry.trim().split(';');
        let Some(media_type) = parts.next().map(str::trim) else {
            continue;
        };
        if media_type.eq_ignore_ascii_case("image/*") {
            let quality = parse_quality(parts);
            image_wildcard = Some(image_wildcard.unwrap_or(0.0_f32).max(quality));
        } else if media_type == "*/*" {
            let quality = parse_quality(parts);
            global_wildcard = Some(global_wildcard.unwrap_or(0.0_f32).max(quality));
        }
    }
    image_wildcard.or(global_wildcard)
}

fn parse_quality<'a>(parameters: impl Iterator<Item = &'a str>) -> f32 {
    for parameter in parameters {
        if let Some((name, value)) = parameter.trim().split_once('=')
            && name.trim().eq_ignore_ascii_case("q")
        {
            return value
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|q| (0.0..=1.0).contains(q))
                .unwrap_or(0.0);
        }
    }
    1.0
}

fn is_fallback(format: RenditionFormat) -> bool {
    matches!(format, RenditionFormat::Jpeg | RenditionFormat::Png)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn length(self) -> u64 {
        self.end - self.start + 1
    }
}

fn parse_range(value: &str, size: u64) -> Result<Option<ByteRange>, ()> {
    let Some(value) = value.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if value.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = value.split_once('-') else {
        return Ok(None);
    };

    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 || size == 0 {
            return Err(());
        }
        let length = suffix.min(size);
        return Ok(Some(ByteRange {
            start: size - length,
            end: size - 1,
        }));
    }

    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some(ByteRange { start, end }))
}

async fn serve_file(
    request_headers: &HeaderMap,
    path: &std::path::Path,
    expected_size: u64,
    sha256: &str,
    mime_type: &str,
    disposition: &str,
    vary_accept: bool,
) -> Result<Response, ApiError> {
    let etag = format!("\"{sha256}\"");
    if etag_matches(request_headers, &etag) {
        let mut response = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, etag)
            .header(CACHE_CONTROL, CACHE_POLICY)
            .body(Body::empty())
            .map_err(ApiError::internal)?;
        if vary_accept {
            response
                .headers_mut()
                .insert(VARY, HeaderValue::from_static("Accept"));
        }
        return Ok(response);
    }

    let range_header = request_headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok());
    let range_is_valid = request_headers
        .get(IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| value == etag);
    let range = if range_is_valid {
        match range_header {
            Some(value) => match parse_range(value, expected_size) {
                Ok(range) => range,
                Err(()) => return range_not_satisfiable(expected_size, vary_accept),
            },
            None => None,
        }
    } else {
        None
    };

    let mut file = File::open(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found()
        } else {
            ApiError::internal(error)
        }
    })?;
    let file_size = file.metadata().await.map_err(ApiError::internal)?.len();
    if file_size != expected_size {
        tracing::error!(path = %path.display(), expected_size, file_size, "asset size does not match metadata");
        return Err(ApiError::internal("asset size mismatch"));
    }

    let (status, content_length) = if let Some(range) = range {
        file.seek(SeekFrom::Start(range.start))
            .await
            .map_err(ApiError::internal)?;
        (StatusCode::PARTIAL_CONTENT, range.length())
    } else {
        (StatusCode::OK, file_size)
    };
    let body = if let Some(range) = range {
        Body::from_stream(ReaderStream::new(file.take(range.length())))
    } else {
        Body::from_stream(ReaderStream::new(file))
    };

    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime_type)
        .header(CONTENT_LENGTH, content_length.to_string())
        .header(
            CONTENT_DISPOSITION,
            format!(
                "{disposition}; filename=\"asset.{extension}\"",
                extension = extension_for_mime(mime_type)
            ),
        )
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(CACHE_CONTROL, CACHE_POLICY)
        .header(ETAG, etag)
        .header(ACCEPT_RANGES, "bytes");
    if let Some(range) = range {
        builder = builder.header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{}", range.start, range.end, file_size),
        );
    }
    if vary_accept {
        builder = builder.header(VARY, "Accept");
    }
    builder.body(body).map_err(ApiError::internal)
}

fn range_not_satisfiable(size: u64, vary_accept: bool) -> Result<Response, ApiError> {
    let mut builder = Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(CONTENT_RANGE, format!("bytes */{size}"))
        .header(CACHE_CONTROL, CACHE_POLICY);
    if vary_accept {
        builder = builder.header(VARY, "Accept");
    }
    builder.body(Body::empty()).map_err(ApiError::internal)
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|values| {
            values.split(',').any(|value| {
                let value = value.trim();
                value == "*" || value == etag || value.strip_prefix("W/") == Some(etag)
            })
        })
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/jxl" => "jxl",
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command, time::Duration};

    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, HeaderValue, Request, StatusCode, header::ACCEPT},
    };
    use tower::ServiceExt;

    use crate::{
        AppState, Config, app,
        ident::AssetId,
        model::{RenditionFormat, RenditionRecord},
    };

    use super::{ByteRange, parse_range, select_format};

    fn rendition(format: RenditionFormat) -> RenditionRecord {
        RenditionRecord {
            max_width: 640,
            width: 640,
            height: 480,
            format,
            bytes: 1024,
            sha256: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn format_negotiation_selects_supported_modern_format_and_fallback() {
        let choices = [
            rendition(RenditionFormat::Jxl),
            rendition(RenditionFormat::Avif),
            rendition(RenditionFormat::Webp),
            rendition(RenditionFormat::Jpeg),
        ];
        let choices: Vec<_> = choices.iter().collect();
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("image/avif, image/webp;q=0.8, image/jpeg;q=0.7"),
        );
        assert_eq!(
            select_format(&headers, &choices).unwrap().format,
            RenditionFormat::Avif
        );

        headers.insert(
            ACCEPT,
            HeaderValue::from_static("image/jxl;q=0.5, image/avif;q=0.9, image/webp;q=0"),
        );
        assert_eq!(
            select_format(&headers, &choices).unwrap().format,
            RenditionFormat::Avif
        );

        headers.insert(ACCEPT, HeaderValue::from_static("image/jpeg"));
        assert_eq!(
            select_format(&headers, &choices).unwrap().format,
            RenditionFormat::Jpeg
        );

        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        assert_eq!(
            select_format(&headers, &choices).unwrap().format,
            RenditionFormat::Jpeg
        );
    }

    #[test]
    fn byte_ranges_support_open_and_suffix_ranges() {
        assert_eq!(
            parse_range("bytes=10-19", 100).unwrap(),
            Some(ByteRange { start: 10, end: 19 })
        );
        assert_eq!(
            parse_range("bytes=90-", 100).unwrap(),
            Some(ByteRange { start: 90, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=-10", 100).unwrap(),
            Some(ByteRange { start: 90, end: 99 })
        );
        assert!(parse_range("bytes=100-", 100).is_err());
    }

    fn test_config(data_dir: PathBuf) -> Config {
        Config {
            listen_addr: "127.0.0.1:0".to_owned(),
            data_dir,
            api_token: "prototype-test-token".to_owned(),
            id_length: 18,
            max_image_bytes: 1024 * 1024,
            max_document_bytes: 1024,
            max_image_pixels: 40_000_000,
            max_image_dimension: 12_000,
            variant_widths: vec![320, 640],
            magick_bin: "magick".to_owned(),
            magick_timeout: Duration::from_secs(45),
            image_processing_timeout: Duration::from_secs(120),
        }
    }

    fn upload_request(content_type: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/assets")
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn upload_and_public_document_delivery_work() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let router = app(state);

        let response = router
            .clone()
            .oneshot(upload_request(
                "application/pdf",
                b"%PDF-1.7\nprototype".to_vec(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let id = response["id"].as_str().unwrap();
        assert_eq!(id.len(), 18, "asset ids use the configured length");
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
            "asset ids are lowercase base36: {id}"
        );
        assert_eq!(response["renditions"].as_array().unwrap().len(), 0);

        let delivery = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/original"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delivery.status(), StatusCode::OK);
        assert_eq!(delivery.headers()["content-type"], "application/pdf");
        assert_eq!(
            delivery.headers()["content-disposition"],
            "inline; filename=\"asset.pdf\""
        );
        let etag = delivery.headers()["etag"].clone();
        assert_eq!(
            &to_bytes(delivery.into_body(), 1024).await.unwrap()[..],
            b"%PDF-1.7\nprototype"
        );

        let head = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/media/{id}/original"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["content-length"], "18");
        assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());

        let partial = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/original"))
                    .header("range", "bytes=5-8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()["content-range"], "bytes 5-8/18");
        assert_eq!(
            &to_bytes(partial.into_body(), 1024).await.unwrap()[..],
            b"1.7\n"
        );

        let not_modified = router
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/original"))
                    .header("if-none-match", etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn upload_enforces_streaming_limit_and_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let router = app(state);

        let too_large = router
            .clone()
            .oneshot(upload_request("application/pdf", vec![b'x'; 1025]))
            .await
            .unwrap();
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let unauthorized = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/assets")
                    .header("content-type", "application/pdf")
                    .body(Body::from("%PDF-1.7"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(unauthorized.headers()["www-authenticate"], "Bearer");
    }

    #[tokio::test]
    async fn raw_or_mismatched_image_upload_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let response = app(state)
            .oneshot(upload_request("image/jpeg", b"II*\0camera raw".to_vec()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn startup_cleans_staging_and_unindexed_assets() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let config = test_config(data_dir.clone());
        let state = AppState::new(config.clone()).await.unwrap();
        drop(state);

        let staged = data_dir
            .join("staging")
            .join(AssetId::generate(18).unwrap().to_string());
        let orphaned = data_dir
            .join("assets")
            .join(AssetId::generate(18).unwrap().to_string());
        tokio::fs::create_dir_all(&staged).await.unwrap();
        tokio::fs::create_dir_all(&orphaned).await.unwrap();
        tokio::fs::write(staged.join("partial"), b"partial")
            .await
            .unwrap();
        tokio::fs::write(orphaned.join("orphan"), b"orphan")
            .await
            .unwrap();

        let _state = AppState::new(config).await.unwrap();
        assert!(!staged.exists());
        assert!(!orphaned.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_non_private_nonempty_storage_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let shared = directory.path().join("shared");
        tokio::fs::create_dir(&shared).await.unwrap();
        tokio::fs::write(shared.join("keep"), b"not Atlas data")
            .await
            .unwrap();
        tokio::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();

        let result = AppState::new(test_config(shared.clone())).await;
        assert!(result.is_err());
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(shared.join("keep").exists());
    }

    #[tokio::test]
    async fn malformed_asset_ids_are_not_found() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let router = app(state);

        for id in [
            "not-a-valid-id",
            "../../etc/passwd",
            "AAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaa",
            "aaaa-aaaaaaaaaaaaa",
            "",
        ] {
            for path in [format!("/media/{id}/original"), format!("/media/{id}/w320")] {
                let response = router
                    .clone()
                    .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            }

            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/v1/assets/{id}"))
                        .header("authorization", "Bearer prototype-test-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "lookup {id}");
        }
    }

    #[tokio::test]
    async fn image_upload_creates_width_renditions_and_negotiates_format() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("source.png");
        let generated = Command::new("magick")
            .args(["-size", "640x480", "xc:red"])
            .arg(format!("PNG:{}", image_path.display()))
            .output()
            .unwrap();
        assert!(generated.status.success());
        let image = tokio::fs::read(&image_path).await.unwrap();
        let state = AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap();
        let router = app(state);

        let uploaded = router
            .clone()
            .oneshot(upload_request("image/png", image))
            .await
            .unwrap();
        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let result: serde_json::Value =
            serde_json::from_slice(&to_bytes(uploaded.into_body(), 8192).await.unwrap()).unwrap();
        let id = result["id"].as_str().unwrap();
        assert_eq!(result["renditions"].as_array().unwrap().len(), 2);
        assert_eq!(result["renditions"][0]["max_width"], 320);
        assert_eq!(result["renditions"][1]["max_width"], 640);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/w320"))
                    .header(ACCEPT, "image/jxl, image/avif;q=0.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/jxl");
        assert_eq!(response.headers()["vary"], "Accept");
        assert!(
            !to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .is_empty()
        );

        let fallback = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/w320"))
                    .header(ACCEPT, "*/*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fallback.status(), StatusCode::OK);
        assert_eq!(fallback.headers()["content-type"], "image/png");

        let unacceptable = router
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/w320"))
                    .header(ACCEPT, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unacceptable.status(), StatusCode::NOT_ACCEPTABLE);
        assert_eq!(unacceptable.headers()["vary"], "Accept");
    }
}

use std::{
    collections::BTreeMap,
    io::SeekFrom,
    path::PathBuf,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
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
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    error::ApiError,
    filename,
    ident::AssetId,
    media::{MediaError, sha256_bytes, sha256_file, validate_signature},
    model::{
        AssetRecord, AssetState, Kind, RenditionFormat, RenditionRecord, SourceFormat, StoredAsset,
    },
    store::{
        DEFAULT_LIST_LIMIT, InsertOutcome, ListQuery, MAX_LIST_LIMIT, Order, Sort, StateFilter,
    },
};

const CACHE_POLICY: &str = "public, max-age=31536000, immutable";
/// For a rendition URL that names no generation. It serves whichever
/// generation is current, so a re-encode changes its bytes, and a year-long
/// immutable copy would hide the fix exactly where it matters. The URLs the
/// API hands out name their generation and are cached as immutable.
const LATEST_CACHE_POLICY: &str = "public, max-age=3600";
/// Deliberately short: the placeholder stands in for an asset that may come back
/// from the trash, and an immutable one could outlive that restore by a year.
const PLACEHOLDER_CACHE_POLICY: &str = "public, max-age=60";
const MAX_ID_ATTEMPTS: usize = 5;

/// The fallback served in place of retired assets. It is a plain image rather
/// than an error so that a page already referencing an asset URL keeps
/// rendering while the asset sits in the trash.
pub(crate) const PLACEHOLDER_PNG: &[u8] = include_bytes!("../assets/placeholder.png");

/// Optional, untrusted, percent-encoded. See [`crate::filename`].
const ORIGINAL_FILENAME_HEADER: &str = "x-original-filename";

static PLACEHOLDER_ETAG: LazyLock<String> =
    LazyLock::new(|| format!("\"{}\"", sha256_bytes(PLACEHOLDER_PNG)));

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
    /// Present only when an upload matched an asset that already existed. The
    /// status says it too; this saves a client from reading a status code for
    /// something a field states directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    deduplicated: Option<bool>,
    /// A consumer's opaque note, exactly as they wrote it.
    ///
    /// Left `None` by `From<&StoredAsset>`, which is what listings and
    /// summaries go through, so a page of fifty assets cannot grow into fifty
    /// blobs. Only the single-asset read attaches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    created_at: i64,
    kind: Kind,
    state: AssetState,
    deleted_at: Option<i64>,
    purged_at: Option<i64>,
    pinned: bool,
    /// Which encoding of the original the rendition URLs name.
    generation: u32,
    original: OriginalResponse,
    renditions: Vec<RenditionResponse>,
}

#[derive(Serialize)]
pub(super) struct OriginalResponse {
    url: String,
    /// The name the uploader gave the file, when it sent one.
    filename: Option<String>,
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

/// A trash listing entry. Purged entries are tombstones: they describe bytes
/// that no longer exist, which is why they carry no rendition list.
#[derive(Serialize)]
pub(super) struct TrashEntryResponse {
    id: String,
    /// Which collection this entry lives in, so a caller listing the whole
    /// trash can address its restore route instead of guessing from the media
    /// type. The documents collection accepts pictures, so a picture's format
    /// does not say where it is.
    kind: Kind,
    state: AssetState,
    created_at: i64,
    deleted_at: Option<i64>,
    purged_at: Option<i64>,
    pinned: bool,
    bytes: u64,
    content_type: &'static str,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Serialize)]
pub(super) struct TrashResponse {
    assets: Vec<TrashEntryResponse>,
    total: i64,
    limit: i64,
    offset: i64,
}

/// A trash page is bounded so the listing cannot grow without limit. Callers
/// page through `offset`; the default covers any realistic single screen.
const DEFAULT_TRASH_LIMIT: i64 = 200;
const MAX_TRASH_LIMIT: i64 = 500;

#[derive(Deserialize)]
pub(super) struct PinRequest {
    pinned: bool,
}

/// A consumer's opaque note. `null` clears it.
#[derive(Deserialize)]
pub(super) struct DataRequest {
    data: Option<String>,
}

/// How many ids one state lookup may carry. A page of photos is the use case;
/// anything larger is a caller bug or an attempt to make Atlas a database.
const MAX_STATE_IDS: usize = 500;

#[derive(Deserialize)]
pub(super) struct TrashQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct StateRequest {
    ids: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct StateEntryResponse {
    id: String,
    state: AssetState,
    /// Which collection it belongs to, so a caller can address it with a typed
    /// route afterwards. Null only when Atlas has no record of the id.
    kind: Option<Kind>,
    pinned: bool,
}

#[derive(Serialize)]
pub(super) struct StateResponse {
    assets: Vec<StateEntryResponse>,
}

impl AssetResponse {
    /// The single-asset read, which is the only place a consumer's own data is
    /// worth the bytes on the wire.
    fn with_data(mut self, data: Option<String>) -> Self {
        self.data = data;
        self
    }

    fn deduplicated(mut self) -> Self {
        self.deduplicated = Some(true);
        self
    }
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
                    url: rendition_url(&asset.id, max_width, asset.generation),
                    formats,
                }
            })
            .collect();

        Self {
            id: asset.id.to_string(),
            deduplicated: None,
            data: None,
            created_at: asset.created_at,
            kind: asset.kind.unwrap_or_else(|| asset.source_format.kind()),
            state: asset.state(),
            deleted_at: asset.deleted_at,
            purged_at: asset.purged_at,
            pinned: asset.pinned,
            generation: asset.generation,
            original: OriginalResponse {
                url: format!("/media/{}/original", asset.id),
                filename: asset.original_name.clone(),
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

/// The public URL of one rendition profile at one generation.
///
/// Always versioned: the bytes behind it never change, so it is the URL a
/// consumer can store and a cache can keep. The bare `/media/{id}/w{width}`
/// still works and serves whichever generation is current.
fn rendition_url(id: &AssetId, max_width: u32, generation: u32) -> String {
    format!("/media/{id}/w{max_width}?g={generation}")
}

impl From<&StoredAsset> for TrashEntryResponse {
    fn from(value: &StoredAsset) -> Self {
        let asset = &value.asset;
        Self {
            id: asset.id.to_string(),
            kind: asset.kind.unwrap_or_else(|| asset.source_format.kind()),
            state: asset.state(),
            created_at: asset.created_at,
            deleted_at: asset.deleted_at,
            purged_at: asset.purged_at,
            pinned: asset.pinned,
            bytes: asset.bytes,
            content_type: asset.source_format.mime_type(),
            width: asset.width,
            height: asset.height,
        }
    }
}

/// Upload into the images collection.
pub(super) async fn upload_image(
    State(state): State<AppState>,
    request: Request,
) -> Result<(StatusCode, Json<AssetResponse>), ApiError> {
    upload_into(state, Kind::Image, request).await
}

/// Upload into the documents collection, which also accepts images that should
/// be stored without being converted.
pub(super) async fn upload_document(
    State(state): State<AppState>,
    request: Request,
) -> Result<(StatusCode, Json<AssetResponse>), ApiError> {
    upload_into(state, Kind::Document, request).await
}

/// The upload itself, shared by both collections.
///
/// The collection comes from the route, so the classification is a property of
/// the call site rather than a header the caller asserts: posting a PDF to
/// `/api/v1/images` is refused, not quietly stored.
async fn upload_into(
    state: AppState,
    kind: Kind,
    request: Request,
) -> Result<(StatusCode, Json<AssetResponse>), ApiError> {
    let state = &state;
    authorize(state, request.headers())?;
    let headers = request.headers();
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::unsupported("Set a supported Content-Type for the file."))?;
    let source_format = SourceFormat::parse_content_type(content_type)
        .filter(|format| kind.accepts(*format))
        .ok_or_else(|| {
            ApiError::unsupported(match kind {
                Kind::Image => format!(
                    "The images collection accepts {}. GET /api/v1/formats lists them.",
                    list_sentence(&accepted_media_types(&accepted_formats(kind)))
                ),
                Kind::Document => {
                    "This file type is not accepted. GET /api/v1/formats lists what is.".to_owned()
                }
            })
        })?;
    // The byte limit follows the format, not the collection: an image stored as
    // a document is never decoded, but keeping one rule is simpler to reason
    // about than a limit that depends on the route.
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
    // Raw UTF-8 or percent-encoded; see `crate::filename`.
    let original_name = match headers.get(ORIGINAL_FILENAME_HEADER) {
        Some(value) => Some(
            filename::from_header_bytes(value.as_bytes())
                .map_err(|error| ApiError::bad_request("invalid_filename", error.to_string()))?,
        ),
        None => None,
    };

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
    let result = create_asset(
        state,
        id,
        kind,
        source_format,
        original_name,
        request.into_body(),
        &staging_dir,
    )
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_dir).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn create_asset(
    state: &AppState,
    id: AssetId,
    kind: Kind,
    source_format: SourceFormat,
    original_name: Option<String>,
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

    // Identical bytes are the same asset, because asset URLs are immutable and
    // content is what identifies one. Checked before any conversion, so a
    // repeated upload costs a hash instead of a full image pass.
    if let Some(existing) = state
        .store
        .find_live_by_sha256(&original_sha256, kind)
        .await
        .map_err(ApiError::internal)?
    {
        let asset = state
            .store
            .get_asset(&existing)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the duplicate asset vanished mid-upload"))?;
        // Nothing was published, so the staged copy goes away with the request.
        fs::remove_dir_all(staging_dir)
            .await
            .map_err(ApiError::internal)?;
        // 200 rather than 201: nothing was created, and saying so is the point.
        return Ok((
            StatusCode::OK,
            Json(AssetResponse::from(&asset).deduplicated()),
        ));
    }

    let (dimensions, renditions) = if kind.generates_renditions() {
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
        original_name,
        kind: Some(kind),
        deleted_at: None,
        purged_at: None,
        pinned: false,
        consumer_data: None,
        generation: 1,
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

    schedule_purge(state);

    Ok((StatusCode::CREATED, Json(AssetResponse::from(&stored))))
}

/// Sweep expired trash in the background. There is no scheduler, so an upload is
/// the only other moment where the sweep can happen.
fn schedule_purge(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = state.purge_expired_trash().await {
            tracing::error!(%error, "opportunistic trash purge failed");
        }
    });
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
    Path((kind, id)): Path<(String, String)>,
) -> Result<Json<AssetResponse>, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let asset = load_asset(&state, &parse_id(&id)?, Some(kind)).await?;
    // One of only two places a consumer's own data is attached; see the field.
    Ok(Json(
        AssetResponse::from(&asset).with_data(asset.asset.consumer_data.clone()),
    ))
}

/// Move an asset to the trash instead of destroying it.
///
/// The soft delete is recorded before the files move, so the window where Atlas
/// refuses to publish is strictly wider than the window where the bytes are
/// still in `assets/`. A crash in between leaves the bytes to be swept into the
/// trash by startup recovery, never a live row with missing files.
pub(super) async fn delete_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let id = parse_id(&id)?;
    load_asset(&state, &id, Some(kind)).await?;

    delete_inner(&state, &id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Move an asset to the trash. Shared with the bulk endpoint so both take the
/// same order of operations, not just the same outcome.
async fn delete_inner(state: &AppState, id: &AssetId) -> Result<(), ApiError> {
    let asset = load_asset(state, id, None).await?;
    if asset.asset.state() != AssetState::Active {
        return Ok(());
    }

    state
        .store
        .mark_deleted(id, crate::unix_now().map_err(ApiError::internal)?)
        .await
        .map_err(ApiError::internal)?;
    let from = state.asset_dir(id);
    if tokio::fs::symlink_metadata(&from).await.is_ok()
        && let Err(error) = fs::rename(&from, state.trash_dir(id)).await
    {
        tracing::error!(
            asset = %id,
            %error,
            "the asset is in the trash but its files were not moved; startup recovery will finish the move"
        );
        return Err(ApiError::internal(error));
    }
    Ok(())
}

pub(super) async fn restore_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let id = parse_id(&id)?;
    load_asset(&state, &id, Some(kind)).await?;

    restore_inner(&state, &id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Bring a trashed asset back. Files first: a crash between the rename and the
/// row update leaves a deleted row over files that are back in `assets/`, which
/// recovery resolves back into the trash.
async fn restore_inner(state: &AppState, id: &AssetId) -> Result<(), ApiError> {
    let asset = load_asset(state, id, None).await?;
    match asset.asset.state() {
        AssetState::Active => return Ok(()),
        AssetState::Purged | AssetState::Missing => {
            return Err(ApiError::conflict(
                "asset_purged",
                "The asset was purged and its files no longer exist.",
            ));
        }
        AssetState::Deleted => {}
    }

    fs::rename(state.trash_dir(id), state.asset_dir(id))
        .await
        .map_err(ApiError::internal)?;
    state
        .store
        .mark_restored(id)
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

/// Record that a consuming application still uses this asset, so purge skips it.
pub(super) async fn set_pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
    Json(body): Json<PinRequest>,
) -> Result<Json<AssetResponse>, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let id = parse_id(&id)?;
    // Checked before the write, so a wrong collection cannot pin an asset that
    // is not in it.
    load_asset(&state, &id, Some(kind)).await?;
    set_pinned_inner(&state, &id, body.pinned).await?;
    let asset = load_asset(&state, &id, Some(kind)).await?;
    Ok(Json(AssetResponse::from(&asset)))
}

/// Store, replace or clear a consumer's own note about an asset.
///
/// Atlas enforces a size limit and nothing else. There is no schema, no
/// validation and no interpretation of the value anywhere on this path: the
/// field was documented as opaque, and reading into it is exactly the thing
/// that would make Atlas a content store.
pub(super) async fn set_data(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
    Json(body): Json<DataRequest>,
) -> Result<Json<AssetResponse>, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let id = parse_id(&id)?;
    if let Some(data) = body.data.as_deref() {
        if data.len() > state.config.max_consumer_data_bytes as usize {
            return Err(ApiError::bad_request(
                "data_too_large",
                format!(
                    "data is {} bytes; the limit is {}.",
                    data.len(),
                    state.config.max_consumer_data_bytes
                ),
            ));
        }
    }
    load_asset(&state, &id, Some(kind)).await?;
    if !state
        .store
        .set_consumer_data(&id, body.data.as_deref())
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::not_found());
    }
    let asset = load_asset(&state, &id, Some(kind)).await?;
    Ok(Json(
        AssetResponse::from(&asset).with_data(asset.asset.consumer_data.clone()),
    ))
}

async fn set_pinned_inner(state: &AppState, id: &AssetId, pinned: bool) -> Result<(), ApiError> {
    if !state
        .store
        .set_pinned(id, pinned)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::not_found());
    }
    Ok(())
}

/// Answer "which of the ids I already know can you still serve?".
///
/// This is not a listing: the caller supplies the ids, so Atlas learns nothing
/// about the consumer's content and the result cannot be paged through. Ids
/// Atlas does not know come back as `missing`, because telling "never existed"
/// apart from "deleted" is the whole reason a consumer asks.
pub(super) async fn asset_states(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StateRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    authorize(&state, &headers)?;

    if body.ids.len() > MAX_STATE_IDS {
        return Err(ApiError::bad_request(
            "too_many_ids",
            format!("Ask about at most {MAX_STATE_IDS} assets at a time."),
        ));
    }

    let mut ids = Vec::with_capacity(body.ids.len());
    for raw in &body.ids {
        let id = AssetId::parse(raw).map_err(|_| {
            ApiError::bad_request(
                "invalid_asset_ids",
                "Every id must be a lowercase base36 asset id of 12 to 24 characters.",
            )
        })?;
        ids.push(id);
    }

    let known = state
        .store
        .asset_states_by_id(&ids)
        .await
        .map_err(ApiError::internal)?;
    let assets = ids
        .into_iter()
        .map(|id| {
            let (state, pinned, kind) =
                known
                    .get(id.as_str())
                    .copied()
                    .unwrap_or((AssetState::Missing, false, None));
            StateEntryResponse {
                id: id.to_string(),
                state,
                pinned,
                kind,
            }
        })
        .collect();

    Ok(Json(StateResponse { assets }))
}

/// A browsable page of one collection.
///
/// Every filter here is a fact Atlas stores; nothing about meaning is
/// filterable, because nothing about meaning is Atlas's business.
pub(super) async fn list_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse>, ApiError> {
    list_collection(state, Kind::Image, headers, params).await
}

pub(super) async fn list_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse>, ApiError> {
    list_collection(state, Kind::Document, headers, params).await
}

/// The listing itself, shared by both collections.
async fn list_collection(
    state: AppState,
    kind: Kind,
    headers: HeaderMap,
    params: ListParams,
) -> Result<Json<ListResponse>, ApiError> {
    let state = &state;
    authorize(state, &headers)?;
    let query = params.into_query(kind)?;

    let (assets, total) = state
        .store
        .list_assets(query.clone())
        .await
        .map_err(ApiError::internal)?;
    // One extra query for the page's renditions, rather than one per asset.
    let renditions = state
        .store
        .renditions_for(
            &assets
                .iter()
                .map(|stored| &stored.asset.id)
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(ApiError::internal)?;
    let assets = assets
        .into_iter()
        .map(|mut stored| {
            stored.renditions = renditions
                .get(stored.asset.id.as_str())
                .cloned()
                .unwrap_or_default();
            AssetResponse::from(&stored)
        })
        .collect();

    Ok(Json(ListResponse {
        assets,
        total,
        limit: query.limit,
        offset: query.offset,
    }))
}

#[derive(Deserialize)]
pub(super) struct ListParams {
    state: Option<String>,
    q: Option<String>,
    created_after: Option<i64>,
    created_before: Option<i64>,
    bytes_min: Option<i64>,
    bytes_max: Option<i64>,
    width_min: Option<i64>,
    height_min: Option<i64>,
    sort: Option<String>,
    order: Option<String>,
    mime: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Turn a `mime` filter into the format keys it selects.
///
/// Two failures, one error code, because a caller who gets either of them
/// wrote the query wrong and wants to be told rather than handed a page that
/// quietly contains the wrong thing. An unrecognised value, and a recognised
/// value that could never match anything in this collection.
fn parse_mime(value: &str, kind: Kind) -> Result<Vec<&'static str>, ApiError> {
    let wanted: Vec<SourceFormat> = match value.split_once("/*") {
        Some((group, "")) => {
            let wanted = SourceFormat::ALL
                .into_iter()
                .filter(|format| format.mime_type().starts_with(&format!("{group}/")))
                .collect::<Vec<_>>();
            if wanted.is_empty() {
                return Err(invalid_mime());
            }
            wanted
        }
        Some(_) => return Err(invalid_mime()),
        None => match SourceFormat::parse_content_type(value) {
            Some(format) => vec![format],
            None => return Err(invalid_mime()),
        },
    };

    // What this collection can hold, which is not the same as the format's
    // usual home: documents take pictures too.
    let selectable = wanted
        .iter()
        .filter(|format| kind.accepts(**format))
        .copied()
        .collect::<Vec<_>>();
    if selectable.is_empty() {
        // The type is real, but nothing of it can be in this collection. Say
        // what the collection does hold, because "images holds image/jpeg,
        // image/png and image/webp" is the sentence the caller needs.
        return Err(ApiError::bad_request(
            "invalid_mime",
            format!(
                "{value} cannot match the {} collection. GET /api/v1/formats lists what it holds.",
                kind.slug(),
            ),
        ));
    }

    Ok(selectable.iter().map(|format| format.key()).collect())
}

fn invalid_mime() -> ApiError {
    ApiError::bad_request(
        "invalid_mime",
        "mime must be an accepted media type, such as image/png, or a group such \
         as image/*. GET /api/v1/formats lists them.",
    )
}

/// A list of things, the way a sentence reads it: "a, b, c and d".
fn list_sentence(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        _ => {
            let (last, rest) = items.split_last().expect("the list is not empty");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

/// Every media type a set of formats may be declared as.
///
/// Every accepted spelling is listed, not just the canonical one: this is
/// where a caller reads what it may put in a `Content-Type`, so a spelling
/// Atlas accepts but does not advertise would be a spelling it rejects for
/// no stated reason.
fn accepted_media_types(formats: &[SourceFormat]) -> Vec<&'static str> {
    formats
        .iter()
        .flat_map(|format| format.accepted_content_types().iter().copied())
        .collect()
}

/// The canonical media type of each format: one entry per format.
///
/// This is the list of *formats*, spelled as the media type each one is
/// stored and served as. A format with more than one accepted spelling
/// appears once here, so counting this list counts formats; the spellings
/// themselves are in `accepted_media_types`.
fn canonical_media_types(formats: &[SourceFormat]) -> Vec<&'static str> {
    formats.iter().map(|format| format.mime_type()).collect()
}

impl ListParams {
    fn into_query(self, kind: Kind) -> Result<ListQuery, ApiError> {
        let state = match self.state.as_deref() {
            None | Some("active") => StateFilter::Live,
            Some("deleted") => StateFilter::Deleted,
            Some("purged") => StateFilter::Purged,
            Some("all") => StateFilter::All,
            Some(_) => {
                return Err(ApiError::bad_request(
                    "invalid_state",
                    "state must be active, deleted, purged or all.",
                ));
            }
        };

        let sort = match self.sort.as_deref() {
            None | Some("created_at") => Sort::CreatedAt,
            Some("bytes") => Sort::Bytes,
            Some("width") => Sort::Width,
            Some("filename") => Sort::Filename,
            Some(_) => {
                return Err(ApiError::bad_request(
                    "invalid_sort",
                    "sort must be created_at, bytes, width or filename.",
                ));
            }
        };
        let order = match self.order.as_deref() {
            None | Some("desc") => Order::Desc,
            Some("asc") => Order::Asc,
            Some(_) => {
                return Err(ApiError::bad_request(
                    "invalid_order",
                    "order must be asc or desc.",
                ));
            }
        };
        let mime = match self.mime.as_deref() {
            None => None,
            Some(value) => Some(parse_mime(value, kind)?),
        };

        Ok(ListQuery {
            kind,
            state,
            mime,
            name_contains: self.q.filter(|value| !value.is_empty()),
            created_after: self.created_after,
            created_before: self.created_before,
            bytes_min: self.bytes_min,
            bytes_max: self.bytes_max,
            width_min: self.width_min,
            height_min: self.height_min,
            sort,
            order,
            limit: self
                .limit
                .unwrap_or(DEFAULT_LIST_LIMIT)
                .clamp(1, MAX_LIST_LIMIT),
            offset: self.offset.unwrap_or(0).max(0),
        })
    }
}

#[derive(Serialize)]
pub(super) struct ListResponse {
    assets: Vec<AssetResponse>,
    total: i64,
    limit: i64,
    offset: i64,
}

/// Counts and bytes per collection, for a library header.
pub(super) async fn summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SummaryResponse>, ApiError> {
    authorize(&state, &headers)?;
    let rows = state.store.summary().await.map_err(ApiError::internal)?;
    let for_kind = |kind: Kind| -> CollectionSummary {
        rows.iter()
            .find(|(stored, _, _)| *stored == kind)
            .map(|(_, count, bytes)| CollectionSummary {
                count: *count,
                bytes: *bytes,
            })
            .unwrap_or_default()
    };
    let images = for_kind(Kind::Image);
    let documents = for_kind(Kind::Document);
    let total = CollectionSummary {
        count: images.count + documents.count,
        bytes: images.bytes + documents.bytes,
    };
    Ok(Json(SummaryResponse {
        images,
        documents,
        total,
        rendition_widths: state.config.variant_widths.clone(),
    }))
}

#[derive(Default, Serialize)]
pub(super) struct CollectionSummary {
    count: i64,
    bytes: i64,
}

/// The formats a collection accepts, in the order they are advertised.
///
/// One list, asked for in two places: the endpoint that tells a consumer what
/// it may send, and the error that tells one what it may not.
fn accepted_formats(kind: Kind) -> Vec<SourceFormat> {
    SourceFormat::ALL
        .into_iter()
        .filter(|format| kind.accepts(*format))
        .collect()
}

/// What each collection accepts.
///
/// Derived from the same enum the upload path validates against, so this cannot
/// describe a list Atlas will then reject: a consumer building an upload form
/// reads it instead of hardcoding types that drift on the next release.
///
/// The two lists answer two different questions, and a consumer that reads
/// one as the other gets a wrong answer rather than an error: `formats` holds
/// one media type per format, so counting it counts formats, while
/// `media_types` holds every spelling a caller may declare, so a format with
/// two names appears twice in it and once in `formats`.
pub(super) async fn list_formats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<FormatsResponse>, ApiError> {
    authorize(&state, &headers)?;
    let for_kind = |kind: Kind| -> AcceptedFormats {
        let formats = accepted_formats(kind);
        // The groups a `mime` filter may name for this collection: the type
        // prefixes that actually appear among the formats above.
        let mut groups = formats
            .iter()
            .filter_map(|format| format.mime_type().split_once('/'))
            .map(|(prefix, _)| format!("{prefix}/*"))
            .collect::<Vec<_>>();
        groups.sort();
        groups.dedup();
        AcceptedFormats {
            formats: canonical_media_types(&formats),
            media_types: accepted_media_types(&formats),
            groups,
            generates_renditions: kind.generates_renditions(),
        }
    };
    Ok(Json(FormatsResponse {
        images: for_kind(Kind::Image),
        documents: for_kind(Kind::Document),
    }))
}

#[derive(Serialize)]
pub(super) struct AcceptedFormats {
    /// One media type per accepted format: the canonical spelling, the one
    /// the asset is stored and served as.
    formats: Vec<&'static str>,
    /// Every media type a caller may declare, which is more than one for a
    /// format with alternative spellings.
    media_types: Vec<&'static str>,
    groups: Vec<String>,
    generates_renditions: bool,
}

#[derive(Serialize)]
pub(super) struct FormatsResponse {
    images: AcceptedFormats,
    documents: AcceptedFormats,
}

#[derive(Serialize)]
pub(super) struct SummaryResponse {
    images: CollectionSummary,
    documents: CollectionSummary,
    total: CollectionSummary,
    /// The widths this instance generates renditions at.
    ///
    /// The configuration is otherwise invisible: a consumer can only find out
    /// by listing every asset and reading the renditions of each one, which
    /// is how a deployment with the wrong widths finds out that every one of
    /// its assets is missing a width its site needs.
    rendition_widths: Vec<u32>,
}

/// Apply an action to many assets at once.
///
/// The whole batch is validated before anything happens: a typo in one id
/// rejects the batch rather than deleting forty files and failing on the
/// forty-first. Once it starts, each id reports its own outcome, because a
/// filesystem operation can still fail halfway and pretending otherwise would be
/// a lie.
pub(super) async fn bulk_actions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(body): Json<ActionRequest>,
) -> Result<Json<ActionsResponse>, ApiError> {
    authorize(&state, &headers)?;
    let kind = parse_kind(&kind)?;
    let action = body.action()?;

    if body.ids.len() > MAX_STATE_IDS {
        return Err(ApiError::bad_request(
            "too_many_ids",
            format!("Act on at most {MAX_STATE_IDS} assets at a time."),
        ));
    }

    let mut ids = Vec::with_capacity(body.ids.len());
    for raw in &body.ids {
        ids.push(AssetId::parse(raw).map_err(|_| {
            ApiError::bad_request(
                "invalid_asset_ids",
                "Every id must be a lowercase base36 asset id of 12 to 24 characters.",
            )
        })?);
    }

    let mut unknown = Vec::new();
    for id in &ids {
        if load_asset(&state, id, Some(kind)).await.is_err() {
            unknown.push(id.to_string());
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::bad_request(
            "unknown_asset",
            format!(
                "No action was taken. These ids are not in the {} collection: {}",
                kind.slug(),
                unknown.join(", ")
            ),
        ));
    }

    let mut results = Vec::with_capacity(ids.len());
    for id in ids {
        let outcome = match action {
            BulkAction::Delete => delete_inner(&state, &id).await.map(|()| "deleted"),
            BulkAction::Restore => restore_inner(&state, &id).await.map(|()| "restored"),
            BulkAction::Pin => set_pinned_inner(&state, &id, true).await.map(|()| "pinned"),
            BulkAction::Unpin => set_pinned_inner(&state, &id, false)
                .await
                .map(|()| "unpinned"),
        };
        results.push(match outcome {
            Ok(status) => ActionResult {
                id: id.to_string(),
                status,
                error: None,
            },
            Err(error) => {
                tracing::error!(asset = %id, "a bulk action failed for one asset");
                ActionResult {
                    id: id.to_string(),
                    status: "error",
                    error: Some(error.detail().to_owned()),
                }
            }
        });
    }

    Ok(Json(ActionsResponse { results }))
}

#[derive(Deserialize)]
pub(super) struct ActionRequest {
    action: String,
    ids: Vec<String>,
}

impl ActionRequest {
    fn action(&self) -> Result<BulkAction, ApiError> {
        match self.action.as_str() {
            "delete" => Ok(BulkAction::Delete),
            "restore" => Ok(BulkAction::Restore),
            "pin" => Ok(BulkAction::Pin),
            "unpin" => Ok(BulkAction::Unpin),
            _ => Err(ApiError::bad_request(
                "unknown_action",
                "action must be delete, restore, pin or unpin.",
            )),
        }
    }
}

#[derive(Clone, Copy)]
enum BulkAction {
    Delete,
    Restore,
    Pin,
    Unpin,
}

#[derive(Serialize)]
pub(super) struct ActionResult {
    id: String,
    status: &'static str,
    error: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ActionsResponse {
    results: Vec<ActionResult>,
}

/// The `src` and `srcset` a consumer can paste, for the images collection.
///
/// Each descriptor is the rendition's *actual* width while its URL keeps the
/// profile it was stored under, which is the distinction a hand-written srcset
/// gets wrong.
pub(super) async fn get_srcset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<SrcsetParams>,
) -> Result<Json<SrcsetResponse>, ApiError> {
    authorize(&state, &headers)?;
    let id = parse_id(&id)?;
    let asset = load_asset(&state, &id, Some(Kind::Image)).await?;
    // Without a `widths` parameter the answer is the instance's own set of
    // widths. The hardcoded default this used to carry was the *default
    // configuration* leaking into the place where the *configuration*
    // belongs: on an instance configured with any other set of widths, the
    // default asked for renditions that do not exist, matched none of them,
    // and answered with no srcset at all.
    let use_configured_widths = params.widths.is_none();
    let widths = params.widths(&state.config.variant_widths)?;
    Ok(Json(srcset_for(&asset, widths, use_configured_widths)))
}

/// The `src` and `srcset` for some widths of an asset's current generation.
fn srcset_for(
    asset: &StoredAsset,
    widths: Vec<u32>,
    use_configured_widths: bool,
) -> SrcsetResponse {
    let id = &asset.asset.id;
    let generation = asset.asset.generation;
    // One entry per actual width: each stored rendition has one row per
    // *format*, and a srcset descriptor describes a width, not a
    // representation. When a configuration has changed, a requested profile
    // can still resolve to an older, narrower one. That is a usable fallback,
    // not an empty srcset, and repeated actual widths have only one useful
    // descriptor.
    let mut chosen: Vec<(u32, &RenditionRecord)> = Vec::new();
    for width in widths {
        let exact = asset
            .renditions
            .iter()
            .find(|rendition| rendition.max_width == width);
        let (profile, rendition) = match exact {
            Some(rendition) => (width, rendition),
            None => {
                if !use_configured_widths {
                    continue;
                }
                let Some(actual) = asset
                    .renditions
                    .iter()
                    .filter(|rendition| rendition.width <= width)
                    .map(|rendition| rendition.width)
                    .max()
                else {
                    continue;
                };
                let Some(rendition) = asset
                    .renditions
                    .iter()
                    .find(|rendition| rendition.width == actual)
                else {
                    continue;
                };
                // The URL handed out names its generation, so its bytes have
                // to be final. A profile the asset is merely missing would
                // get different bytes once it is added; the narrower
                // profile that was stored will not.
                if is_settled(width, rendition.width, false, asset.asset.width) {
                    (width, rendition)
                } else {
                    (rendition.max_width, rendition)
                }
            }
        };
        if let Some((requested, _)) = chosen
            .iter_mut()
            .find(|(_, present)| present.width == rendition.width)
        {
            // Keep the widest requested profile for this actual width. It
            // resolves to the same stored bytes and makes `src` useful even
            // when the configured profiles exceed what a small image can
            // provide.
            *requested = (*requested).max(profile);
        } else {
            chosen.push((profile, rendition));
        }
    }
    chosen.sort_by_key(|(requested, _)| *requested);

    // The fallback `src` is the widest width that was asked for and exists, or
    // the original when none of the requested widths were generated.
    let src = match chosen.last() {
        Some((requested, _)) => rendition_url(id, *requested, generation),
        None => format!("/media/{id}/original"),
    };

    let srcset = (!chosen.is_empty()).then(|| {
        chosen
            .iter()
            .map(|(requested, rendition)| {
                format!(
                    "{} {}w",
                    rendition_url(id, *requested, generation),
                    rendition.width
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    });

    SrcsetResponse {
        src,
        srcset,
        generation,
    }
}

#[derive(Deserialize)]
pub(super) struct SrcsetParams {
    widths: Option<String>,
}

impl SrcsetParams {
    /// The widths to describe.
    ///
    /// A caller may name its own; otherwise the instance's configured widths
    /// are the answer, because those are the widths the assets actually have
    /// renditions for.
    fn widths(&self, configured: &[u32]) -> Result<Vec<u32>, ApiError> {
        let Some(raw) = self.widths.as_deref() else {
            return Ok(configured.to_vec());
        };
        let widths: Vec<u32> = raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.parse::<u32>())
            .collect::<Result<_, _>>()
            .map_err(|_| {
                ApiError::bad_request(
                    "invalid_widths",
                    "widths must be a comma-separated list of positive integers.",
                )
            })?;
        if widths.is_empty() || widths.len() > 8 || widths.contains(&0) {
            return Err(ApiError::bad_request(
                "invalid_widths",
                "Ask for between 1 and 8 positive widths.",
            ));
        }
        Ok(widths)
    }
}

#[derive(Serialize)]
pub(super) struct SrcsetResponse {
    src: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    srcset: Option<String>,
    /// The generation every URL here names.
    generation: u32,
}

/// Generate the renditions an asset is missing.
///
/// The widths an asset has are fixed when it is uploaded, and a deployment
/// that configures the wrong widths leaves every asset unable to answer the
/// widths its site needs. Nothing about that is recoverable from the outside:
/// uploads are content-addressed, so the same bytes return the same asset,
/// and uploading again under a new id leaves every reference the consumer
/// holds pointing at a tombstone.
///
/// This is the repair. The original bytes do not change, so the asset id does
/// not have to change either, and a consumer's references keep working.
///
/// What it does *not* do is replace a rendition that is already there. A
/// public URL that changes its bytes is a lie to every cache that holds it,
/// and the pages that reference it would keep the old copy for as long as
/// their cache policy says. So the widths that exist are left exactly as
/// they are, and only the missing ones are generated — which is what the
/// mistake this repairs needs.
pub(super) async fn refresh_renditions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<RenditionRefreshResponse>, ApiError> {
    authorize(&state, &headers)?;
    let request: RenditionsRequest = optional_json(&body)?;
    let id = parse_id(&id)?;
    // Reject an unknown id before waiting behind another conversion.
    load_asset(&state, &id, Some(Kind::Image)).await?;
    if request.re_encode {
        return re_encode(&state, &id).await.map(Json);
    }
    // Serialise conversions and re-renders: the output paths are named after
    // actual widths, and two requests must not both share a staging directory
    // or promote a file there.
    let _permit = state
        .conversion_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(ApiError::internal)?;

    // Reload after waiting for the conversion slot: another re-render may
    // have added the profiles while this request was queued.
    let asset = load_asset(&state, &id, Some(Kind::Image)).await?;
    if asset.asset.state() != AssetState::Active {
        // The bytes of a retired asset are in the trash, and a rendition is
        // something an asset has while it is alive.
        return Err(ApiError::conflict(
            "asset_retired",
            "The asset is in the trash; restore it before generating renditions for it.",
        ));
    }

    let source_format = asset.asset.source_format;
    let original = state
        .asset_dir(&id)
        .join(format!("original.{}", source_format.extension()));

    // The staging directory is where the new files are written before they
    // are moved into place. A previous failed request can leave it behind;
    // it is private temporary data, so clear it before beginning this run.
    let staging = fresh_staging_dir(&state, &id).await?;

    let dimensions = match state
        .image_processor
        .inspect(&original, source_format, &staging)
        .await
    {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging).await;
            return Err(map_media_error(error));
        }
    };

    // A configured profile is metadata, even when a small image means that
    // several profiles all resolve to the same actual width. Add rows for
    // every profile. Existing actual widths reuse their existing files;
    // only wholly new actual widths go through ImageMagick.
    let wanted = implied_widths(&state.config.variant_widths, dimensions.width);
    let present_profiles: Vec<u32> = asset
        .renditions
        .iter()
        .map(|rendition| rendition.max_width)
        .collect();
    let missing: Vec<(u32, u32)> = wanted
        .into_iter()
        .filter(|(profile, _)| !present_profiles.contains(profile))
        .collect();

    if !missing.is_empty() {
        let mut to_generate: Vec<u32> = Vec::new();
        for (_, actual) in &missing {
            let already_stored = asset
                .renditions
                .iter()
                .any(|rendition| rendition.width == *actual);
            if !already_stored
                && !to_generate
                    .iter()
                    .any(|profile| (*profile).min(dimensions.width) == *actual)
            {
                to_generate.push(missing.iter().find(|(_, width)| width == actual).unwrap().0);
            }
        }

        let generated = if to_generate.is_empty() {
            Vec::new()
        } else {
            match tokio::time::timeout(
                state.config.image_processing_timeout,
                state.image_processor.generate(
                    &original,
                    source_format,
                    dimensions,
                    &to_generate,
                    &staging,
                ),
            )
            .await
            {
                Ok(Ok(records)) => records,
                Ok(Err(error)) => {
                    let _ = fs::remove_dir_all(&staging).await;
                    return Err(map_media_error(error));
                }
                Err(_) => {
                    let _ = fs::remove_dir_all(&staging).await;
                    return Err(ApiError::invalid(
                        "Image processing exceeded its time limit.",
                    ));
                }
            }
        };
        // A width added to a re-encoded asset belongs to its generation, and
        // its files must not take the name an older generation's file has.
        let mut generated = generated;
        if let Err(error) =
            name_for_generation(&staging, &mut generated, asset.asset.generation).await
        {
            let _ = fs::remove_dir_all(&staging).await;
            return Err(ApiError::internal(error));
        }

        let mut additions = Vec::new();
        for (profile, actual) in &missing {
            let generated_records: Vec<&RenditionRecord> = generated
                .iter()
                .filter(|record| record.width == *actual)
                .collect();
            let source_records: Vec<&RenditionRecord> = if !generated_records.is_empty() {
                generated_records
            } else {
                asset
                    .renditions
                    .iter()
                    .filter(|record| record.width == *actual)
                    .collect()
            };
            for source in source_records {
                let mut alias = source.clone();
                alias.max_width = *profile;
                additions.push(alias);
            }
        }

        // The files are moved into the asset directory before the rows that
        // name them are written, so a crash in between leaves files nobody
        // points at rather than rows whose files are not there. The names
        // are after actual widths the asset does not have yet, so nothing is
        // overwritten. Existing actual widths are aliases to their current
        // file names, not regenerated output.
        let asset_dir = state.asset_dir(&id);
        let mut promoted = Vec::with_capacity(generated.len());
        for rendition in &generated {
            let from = staging.join(&rendition.file_name);
            let to = asset_dir.join(&rendition.file_name);
            if let Err(error) = fs::rename(&from, &to).await {
                for (moved_from, moved_to) in promoted {
                    let _ = fs::rename(moved_to, moved_from).await;
                }
                let _ = fs::remove_dir_all(&staging).await;
                return Err(ApiError::internal(error));
            }
            promoted.push((from, to));
        }

        if let Err(error) = state.store.insert_renditions(&id, &additions).await {
            // The rows did not go in, so the files must not stay: they would
            // be files nobody points at, taking up space for nothing.
            for (from, to) in promoted {
                let _ = fs::rename(to, from).await;
            }
            let _ = fs::remove_dir_all(&staging).await;
            return Err(ApiError::internal(error));
        }
    }

    let mut added: Vec<u32> = missing.iter().map(|(profile, _)| *profile).collect();
    added.sort_unstable();
    added.dedup();

    // The staging directory has served its purpose. Startup sweeps it too,
    // but there is no reason to leave it behind in the meantime.
    let _ = fs::remove_dir_all(&staging).await;

    let asset = load_asset(&state, &id, Some(Kind::Image)).await?;
    Ok(Json(refresh_response(&state, &asset, added)))
}

/// What both modes of the renditions operation answer with.
fn refresh_response(
    state: &AppState,
    asset: &StoredAsset,
    added: Vec<u32>,
) -> RenditionRefreshResponse {
    RenditionRefreshResponse {
        id: asset.asset.id.to_string(),
        generation: asset.asset.generation,
        widths: state.config.variant_widths.clone(),
        added,
        renditions: AssetResponse::from(asset).renditions,
        srcset: srcset_for(asset, state.config.variant_widths.clone(), true),
    }
}

/// A private, empty staging directory for one asset's conversion.
///
/// A previous failed request can leave one behind; it is temporary data, so
/// it is cleared before a run begins.
async fn fresh_staging_dir(state: &AppState, id: &AssetId) -> Result<PathBuf, ApiError> {
    let staging = state.staging_dir(id);
    match fs::remove_dir_all(&staging).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ApiError::internal(error)),
    }
    fs::create_dir(&staging).await.map_err(ApiError::internal)?;
    Ok(staging)
}

/// The name a rendition file has in a given generation.
///
/// The first generation keeps the names every file has had since the start,
/// `w640.webp`; later ones carry the generation, `w640-g2.webp`, so a new
/// encoding is written beside the old one instead of over it.
fn generation_file_name(name: &str, generation: u32) -> String {
    if generation <= 1 {
        return name.to_owned();
    }
    match name.split_once('.') {
        Some((stem, extension)) => format!("{stem}-g{generation}.{extension}"),
        None => format!("{name}-g{generation}"),
    }
}

/// Rename freshly generated files in staging to their generation's names.
async fn name_for_generation(
    staging: &std::path::Path,
    renditions: &mut [RenditionRecord],
    generation: u32,
) -> std::io::Result<()> {
    for rendition in renditions {
        let name = generation_file_name(&rendition.file_name, generation);
        if name != rendition.file_name {
            fs::rename(staging.join(&rendition.file_name), staging.join(&name)).await?;
            rendition.file_name = name;
        }
    }
    Ok(())
}

/// Re-encode every rendition of an image from its original, as a new
/// generation.
///
/// This is the way out of an encoding that turned out to be bad. The asset id
/// does not change, so a consumer's references keep working, and the old
/// generation's files and rows are kept, so every URL that names it keeps the
/// bytes it was handed out with. What moves is the current generation: the
/// URLs the API hands out from now on name the new one, and the bare URL
/// serves it.
///
/// Every profile the asset has is re-encoded, and every configured width it
/// lacks is added: a new generation is what the instance would make of the
/// original today. Bounded by the conversion slot like every other
/// conversion, and staged then promoted, so a failure leaves the current
/// generation exactly as it was.
async fn re_encode(state: &AppState, id: &AssetId) -> Result<RenditionRefreshResponse, ApiError> {
    let _permit = state
        .conversion_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(ApiError::internal)?;
    let asset = load_asset(state, id, Some(Kind::Image)).await?;
    if asset.asset.state() != AssetState::Active {
        return Err(ApiError::conflict(
            "asset_retired",
            "The asset is in the trash; restore it before re-encoding it.",
        ));
    }

    let staging = fresh_staging_dir(state, id).await?;
    let result = re_encode_staged(state, &asset, &staging).await;
    // Startup sweeps staging too, but there is no reason to leave it behind.
    let _ = fs::remove_dir_all(&staging).await;
    let added = result?;

    let asset = load_asset(state, id, Some(Kind::Image)).await?;
    Ok(refresh_response(state, &asset, added))
}

/// The conversion and the promotion of a re-encode, answering with the
/// profiles the previous generation did not have.
async fn re_encode_staged(
    state: &AppState,
    asset: &StoredAsset,
    staging: &std::path::Path,
) -> Result<Vec<u32>, ApiError> {
    let id = &asset.asset.id;
    let source_format = asset.asset.source_format;
    let original = state
        .asset_dir(id)
        .join(format!("original.{}", source_format.extension()));
    let generation = asset.asset.generation + 1;

    let mut profiles: Vec<u32> = asset
        .renditions
        .iter()
        .map(|rendition| rendition.max_width)
        .chain(state.config.variant_widths.iter().copied())
        .collect();
    profiles.sort_unstable();
    profiles.dedup();

    let (dimensions, mut generated) =
        tokio::time::timeout(state.config.image_processing_timeout, async {
            let dimensions = state
                .image_processor
                .inspect(&original, source_format, staging)
                .await?;
            let generated = state
                .image_processor
                .generate(&original, source_format, dimensions, &profiles, staging)
                .await?;
            Ok::<_, MediaError>((dimensions, generated))
        })
        .await
        .map_err(|_| ApiError::invalid("Image processing exceeded its time limit."))?
        .map_err(map_media_error)?;
    name_for_generation(staging, &mut generated, generation)
        .await
        .map_err(ApiError::internal)?;

    // One row per profile, as the repair writes them: a profile that shares
    // its actual width with a narrower one points at that one's files.
    let mut rows = Vec::new();
    for (profile, actual) in implied_widths(&profiles, dimensions.width) {
        for record in generated.iter().filter(|record| record.width == actual) {
            let mut row = record.clone();
            row.max_width = profile;
            rows.push(row);
        }
    }

    // The names carry the new generation, so nothing that is published is
    // overwritten, and taking the files back out on a failure touches only
    // what this run put there.
    let asset_dir = state.asset_dir(id);
    let mut promoted = Vec::with_capacity(generated.len());
    let mut outcome = Ok(());
    for record in &generated {
        let to = asset_dir.join(&record.file_name);
        if let Err(error) = fs::rename(staging.join(&record.file_name), &to).await {
            outcome = Err(ApiError::internal(error));
            break;
        }
        promoted.push(&record.file_name);
    }
    if outcome.is_ok() {
        outcome = match state
            .store
            .supersede_renditions(id, asset.asset.generation, &rows)
            .await
        {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(ApiError::conflict(
                "asset_changed",
                "The asset changed while it was being re-encoded; nothing was replaced.",
            )),
            Err(error) => Err(ApiError::internal(error)),
        };
    }
    if let Err(error) = outcome {
        // A delete does not wait for the conversion slot, so the directory
        // may have moved to the trash since the files went in. They are
        // taken out of wherever it is now; nothing else has these names.
        for name in promoted {
            let _ = fs::remove_file(asset_dir.join(name)).await;
            let _ = fs::remove_file(state.trash_dir(id).join(name)).await;
        }
        return Err(error);
    }

    let previous: Vec<u32> = asset
        .renditions
        .iter()
        .map(|rendition| rendition.max_width)
        .collect();
    Ok(profiles
        .into_iter()
        .filter(|profile| !previous.contains(profile))
        .collect())
}

/// The body of `POST /api/v1/images/{id}/renditions`. An empty body is the
/// repair that adds the missing widths.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RenditionsRequest {
    /// Re-encode every rendition from the original, as a new generation.
    #[serde(default)]
    re_encode: bool,
}

/// An optional JSON body: an absent, empty or `null` one is the default
/// request.
///
/// Unknown fields are refused by the request types, so a misspelt option is
/// a `400` rather than the other mode quietly running instead.
fn optional_json<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice::<Option<T>>(body)
        .map(Option::unwrap_or_default)
        .map_err(|error| {
            ApiError::bad_request(
                "invalid_request_body",
                format!("The request body is not valid for this operation: {error}"),
            )
        })
}

/// How many images one re-render call re-encodes, by default and at most.
///
/// Each one is a full conversion, so a page is kept small enough that a
/// request finishes well inside a proxy's timeout; a caller walks the whole
/// collection by passing `next` back as `after`.
const DEFAULT_RE_RENDER_LIMIT: i64 = 10;
const MAX_RE_RENDER_LIMIT: i64 = 100;

/// Re-encode every live image, a page at a time.
///
/// An encoder change is not one asset, it is every asset: a newer
/// ImageMagick, a newer libjxl, a corrected flag. Each image is re-encoded on
/// its own, taking the conversion slot for that image only, so an upload
/// waits for one conversion rather than for the whole collection. A page
/// answers with how many it re-encoded, which ones failed and why, and how
/// many are left after it; there is no job queue, so the caller drives the
/// walk and sees the progress.
pub(super) async fn re_render_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ReRenderResponse>, ApiError> {
    authorize(&state, &headers)?;
    let request: ReRenderRequest = optional_json(&body)?;
    let limit = request
        .limit
        .unwrap_or(DEFAULT_RE_RENDER_LIMIT)
        .clamp(1, MAX_RE_RENDER_LIMIT);
    let after = request
        .after
        .as_deref()
        .map(|value| {
            AssetId::parse(value).map_err(|_| {
                ApiError::bad_request("invalid_cursor", "after must be an asset id from next.")
            })
        })
        .transpose()?;

    let (ids, remaining) = state
        .store
        .live_images_after(after.as_ref(), limit)
        .await
        .map_err(ApiError::internal)?;

    let mut re_encoded = Vec::with_capacity(ids.len());
    let mut failures = Vec::new();
    for id in &ids {
        match re_encode(&state, id).await {
            Ok(refreshed) => {
                tracing::info!(asset = %id, generation = refreshed.generation, "re-encoded an image");
                re_encoded.push(ReEncodedImage {
                    id: id.to_string(),
                    generation: refreshed.generation,
                });
            }
            Err(error) => {
                tracing::error!(asset = %id, error = error.detail(), "could not re-encode an image");
                failures.push(ActionResult {
                    id: id.to_string(),
                    status: "error",
                    error: Some(error.detail().to_owned()),
                });
            }
        }
    }
    tracing::info!(
        count = re_encoded.len(),
        failed = failures.len(),
        remaining,
        "re-render page done"
    );

    Ok(Json(ReRenderResponse {
        count: re_encoded.len(),
        re_encoded,
        failures,
        next: (remaining > 0)
            .then(|| ids.last().map(AssetId::to_string))
            .flatten(),
        remaining,
    }))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReRenderRequest {
    limit: Option<i64>,
    /// The `next` of the previous page; absent to start from the beginning.
    after: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ReEncodedImage {
    id: String,
    generation: u32,
}

#[derive(Serialize)]
pub(super) struct ReRenderResponse {
    /// How many images this page re-encoded.
    count: usize,
    re_encoded: Vec<ReEncodedImage>,
    failures: Vec<ActionResult>,
    /// Where the next page starts; `null` once the walk is done.
    next: Option<String>,
    /// Live images after this page, not yet visited by this walk.
    remaining: i64,
}

/// The widths a configuration implies, and the width each one produces.
///
/// A profile is a maximum, so a photo narrower than the profile is stored at
/// its own width, and several profiles can end up asking for the same one. The
/// narrowest profile that asks for a width is the one it is stored under, and
/// the rest resolve to it when they are read.
fn implied_widths(configured: &[u32], source_width: u32) -> Vec<(u32, u32)> {
    configured
        .iter()
        .map(|profile| (*profile, (*profile).min(source_width)))
        .collect()
}

#[derive(Serialize)]
pub(super) struct RenditionRefreshResponse {
    id: String,
    /// The generation the renditions are at now.
    generation: u32,
    /// The widths the instance generates.
    widths: Vec<u32>,
    /// The widths this call added. Empty when the asset already had them all.
    added: Vec<u32>,
    /// The asset's renditions, as they are now.
    renditions: Vec<RenditionResponse>,
    /// The srcset for the configured widths, so a consumer that stores one
    /// learns the new URLs from this answer.
    srcset: SrcsetResponse,
}

pub(super) async fn list_trash(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TrashQuery>,
) -> Result<Json<TrashResponse>, ApiError> {
    authorize(&state, &headers)?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRASH_LIMIT)
        .clamp(1, MAX_TRASH_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);

    let (assets, total) = tokio::try_join!(
        state.store.trashed_assets(limit, offset),
        state.store.trashed_count(),
    )
    .map_err(ApiError::internal)?;

    Ok(Json(TrashResponse {
        assets: assets.iter().map(TrashEntryResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Load an asset, optionally insisting on the collection it belongs to.
///
/// An asset in the other collection is `404`, not `409`: from the caller's side
/// there is no image or document with that id, and answering differently would
/// turn the collection into an oracle for what exists.
async fn load_asset(
    state: &AppState,
    id: &AssetId,
    kind: Option<Kind>,
) -> Result<StoredAsset, ApiError> {
    let asset = state
        .store
        .get_asset(id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::not_found)?;
    if let Some(kind) = kind
        && asset.asset.kind.is_some_and(|stored| stored != kind)
    {
        return Err(ApiError::not_found());
    }
    Ok(asset)
}

pub(super) async fn get_original(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let asset = load_asset(&state, &id, None).await?;
    if let Some(response) = serve_placeholder_if_retired(&headers, &asset) {
        return Ok(response);
    }
    let path = state.asset_dir(&id).join(format!(
        "original.{}",
        asset.asset.source_format.extension()
    ));
    let disposition = disposition_for(
        asset.asset.source_format,
        asset
            .asset
            .kind
            .unwrap_or_else(|| asset.asset.source_format.kind()),
    );
    serve_file(
        &headers,
        &path,
        asset.asset.bytes,
        &asset.asset.sha256,
        asset.asset.original_name.as_deref(),
        asset.asset.source_format.mime_type(),
        disposition,
        false,
        CACHE_POLICY,
    )
    .await
}

pub(super) async fn get_rendition(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<RenditionQuery>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let max_width = name
        .strip_prefix('w')
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(ApiError::not_found)?;
    let asset = load_asset(&state, &id, None).await?;
    let requested = query
        .format
        .as_deref()
        .map(parse_rendition_format)
        .transpose()?;
    let generation = query.g.as_deref().map(parse_generation).transpose()?;
    if let Some(response) = serve_placeholder_if_retired(&headers, &asset) {
        return Ok(response);
    }

    // A URL that names an older generation is answered from that
    // generation's rows, which a re-encode kept for exactly this.
    let superseded;
    let renditions = match generation {
        Some(generation) if generation != asset.asset.generation => {
            superseded = state
                .store
                .superseded_renditions(&id, generation)
                .await
                .map_err(ApiError::internal)?;
            if superseded.is_empty() {
                return Err(ApiError::generation_not_found(
                    generation,
                    asset.asset.generation,
                ));
            }
            &superseded
        }
        _ => &asset.renditions,
    };

    let (selected, path, settled) = resolve_rendition(
        &state,
        &asset.asset.id,
        renditions,
        asset.asset.width,
        &headers,
        max_width,
        requested,
    )?;
    serve_file(
        &headers,
        &path,
        selected.bytes,
        &selected.sha256,
        None,
        selected.format.mime_type(),
        "inline",
        // A URL that names its format already says which representation it
        // is, so one copy per URL is enough and the response does not vary by
        // the `Accept` header.
        requested.is_none(),
        // The bytes of a generation never change; the bare URL follows
        // whichever generation is current, and so does a profile the
        // generation is missing, which it may still gain.
        if generation.is_some() && settled {
            CACHE_POLICY
        } else {
            LATEST_CACHE_POLICY
        },
    )
    .await
}

/// The optional `format` parameter of a rendition URL.
#[derive(Deserialize)]
pub(super) struct RenditionQuery {
    /// The format the caller wants, named the way Atlas names it in a
    /// response: `jxl`, `avif`, `webp`, `jpeg`, `png`.
    format: Option<String>,
    /// The generation the URL was handed out for. Absent means the current
    /// one, whichever that is.
    g: Option<String>,
}

/// A generation a caller may name: a positive integer.
fn parse_generation(value: &str) -> Result<u32, ApiError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|generation| *generation > 0)
        .ok_or_else(|| ApiError::bad_request("invalid_generation", "g must be a positive integer."))
}

/// A format name a caller may ask for.
fn parse_rendition_format(value: &str) -> Result<RenditionFormat, ApiError> {
    RenditionFormat::from_key(&value.trim().to_ascii_lowercase()).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_format",
            "format must be one of jxl, avif, webp, jpeg or png.",
        )
    })
}

/// Choose the stored representation for a `/w{max_width}` request.
///
/// A rendition URL asks for an image no wider than `max_width`, which is what
/// the profile list promises. The exact profile is used when it exists;
/// otherwise the widest stored rendition that is no wider than the request is
/// served. That keeps every configured profile reachable for a photo smaller
/// than the largest one — a 500-pixel photo asked for `w960` gets its 500-pixel
/// rendition, not a blank response — and it never upscales.
///
/// The same fallback covers profiles that generation deduplicated: when several
/// requested widths produce one actual width, only the first is stored, and the
/// skipped profiles resolve here.
///
/// A caller that names a format gets that one, whatever the `Accept` header
/// says: the header is for a browser that cannot choose for itself, and a
/// crawler fetching an image for a marketplace needs a format the marketplace
/// accepts, not the newest one the server has.
///
/// Also answers whether the choice is final; see [`is_settled`].
fn resolve_rendition<'a>(
    state: &AppState,
    id: &AssetId,
    renditions: &'a [RenditionRecord],
    source_width: Option<u32>,
    headers: &HeaderMap,
    max_width: u32,
    requested: Option<RenditionFormat>,
) -> Result<(&'a RenditionRecord, std::path::PathBuf, bool), ApiError> {
    let exact: Vec<_> = renditions
        .iter()
        .filter(|rendition| rendition.max_width == max_width)
        .collect();
    let is_exact = !exact.is_empty();
    let group: Vec<&RenditionRecord> = {
        if is_exact {
            exact
        } else {
            let widest = renditions
                .iter()
                .filter(|rendition| rendition.width <= max_width)
                .map(|rendition| rendition.width)
                .max();
            let Some(widest) = widest else {
                let mut available: Vec<u32> = renditions
                    .iter()
                    .map(|rendition| rendition.max_width)
                    .collect();
                available.sort_unstable();
                available.dedup();
                return Err(ApiError::rendition_not_found(max_width, &available));
            };
            // Every stored representation of that one width, so format
            // negotiation still chooses between them.
            renditions
                .iter()
                .filter(|rendition| rendition.width == widest)
                .collect()
        }
    };

    let selected = match requested {
        Some(format) => group
            .iter()
            .copied()
            .find(|rendition| rendition.format == format)
            .ok_or_else(|| ApiError::format_not_found(format, &group))?,
        None => select_format(headers, &group).ok_or_else(ApiError::not_acceptable)?,
    };
    let path = state.asset_dir(id).join(&selected.file_name);
    let settled = is_settled(max_width, selected.width, is_exact, source_width);
    Ok((selected, path, settled))
}

/// Whether what a profile resolves to is final within its generation.
///
/// A profile with its own rows is. One that falls back to a narrower
/// rendition is final only when that rendition is what the profile would
/// produce anyway, because the source is no wider: otherwise adding the
/// profile later puts different bytes behind the same URL.
fn is_settled(profile: u32, served_width: u32, exact: bool, source_width: Option<u32>) -> bool {
    exact || source_width.is_some_and(|source| served_width == profile.min(source))
}

/// How a file is presented when it is served.
///
/// A document is an attachment, so a browser offers to save it rather than
/// rendering it in place — including an image that was uploaded to the
/// documents collection, which is the whole point of putting it there. A PDF is
/// the exception: browsers display one inline, and a link to a brochure or a
/// form means "show me this".
fn disposition_for(source_format: SourceFormat, kind: Kind) -> &'static str {
    match (kind, source_format) {
        (_, SourceFormat::Pdf) => "inline",
        (Kind::Document, _) => "attachment",
        (Kind::Image, _) => "inline",
    }
}

/// Public delivery for a trashed or purged asset.
///
/// The URL keeps working so that pages already referencing it never break, but
/// the bytes are gone from Atlas's point of view. The placeholder is cached
/// briefly rather than immutably, because a restore can bring the asset back.
fn serve_placeholder_if_retired(headers: &HeaderMap, asset: &StoredAsset) -> Option<Response> {
    let state = asset.asset.state();
    if !state.is_retired() {
        return None;
    }
    tracing::debug!(asset = %asset.asset.id, ?state, "serving the placeholder for a retired asset");
    Some(serve_placeholder(headers))
}

fn serve_placeholder(request_headers: &HeaderMap) -> Response {
    let etag = PLACEHOLDER_ETAG.clone();
    if etag_matches(request_headers, &etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, etag)
            .header(CACHE_CONTROL, PLACEHOLDER_CACHE_POLICY)
            .body(Body::empty())
            .expect("static placeholder response is well formed");
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "image/png")
        .header(CONTENT_LENGTH, PLACEHOLDER_PNG.len().to_string())
        .header(CONTENT_DISPOSITION, "inline; filename=\"placeholder.png\"")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(CACHE_CONTROL, PLACEHOLDER_CACHE_POLICY)
        .header(ETAG, etag)
        .header(ACCEPT_RANGES, "bytes")
        .body(Body::from(PLACEHOLDER_PNG))
        .expect("static placeholder response is well formed")
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

/// A collection segment. Anything that is not a collection is not a route.
fn parse_kind(value: &str) -> Result<Kind, ApiError> {
    Kind::from_slug(value).ok_or_else(ApiError::not_found)
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

#[allow(clippy::too_many_arguments)]
async fn serve_file(
    request_headers: &HeaderMap,
    path: &std::path::Path,
    expected_size: u64,
    sha256: &str,
    original_name: Option<&str>,
    mime_type: &str,
    disposition: &str,
    vary_accept: bool,
    cache_policy: &'static str,
) -> Result<Response, ApiError> {
    let etag = format!("\"{sha256}\"");
    if etag_matches(request_headers, &etag) {
        let mut response = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, etag)
            .header(CACHE_CONTROL, cache_policy)
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
                Err(()) => return range_not_satisfiable(expected_size, vary_accept, cache_policy),
            },
            None => None,
        }
    } else {
        None
    };

    let mut file = File::open(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // Metadata says these bytes exist, so this is a damaged data
            // directory rather than a request for something that never did.
            tracing::error!(
                path = %path.display(),
                "metadata records this file but it is missing from storage"
            );
            ApiError::internal("asset file missing from storage")
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

    // A stored name is already sanitized, but the value is only trusted once it
    // has been through the header parser.
    let content_disposition =
        filename::disposition(disposition, original_name, extension_for_mime(mime_type));
    let content_disposition = HeaderValue::from_str(&content_disposition).unwrap_or_else(|_| {
        tracing::warn!("falling back to a generic download name");
        HeaderValue::from_static("inline; filename=\"asset.bin\"")
    });

    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime_type)
        .header(CONTENT_LENGTH, content_length.to_string())
        .header(CONTENT_DISPOSITION, content_disposition)
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(CACHE_CONTROL, cache_policy)
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

fn range_not_satisfiable(
    size: u64,
    vary_accept: bool,
    cache_policy: &'static str,
) -> Result<Response, ApiError> {
    let mut builder = Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(CONTENT_RANGE, format!("bytes */{size}"))
        .header(CACHE_CONTROL, cache_policy);
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
        Router,
        body::{Body, to_bytes},
        http::{HeaderMap, HeaderValue, Request, StatusCode, header::ACCEPT},
    };
    use tower::ServiceExt;

    use crate::{
        AppState, Config, app,
        ident::AssetId,
        model::{RenditionFormat, RenditionRecord, SourceFormat},
    };

    use serde_json::json;

    use super::{ByteRange, ORIGINAL_FILENAME_HEADER, PLACEHOLDER_PNG, parse_range, select_format};

    fn rendition(format: RenditionFormat) -> RenditionRecord {
        RenditionRecord {
            max_width: 640,
            width: 640,
            height: 480,
            format,
            bytes: 1024,
            sha256: "deadbeef".to_owned(),
            file_name: "w640.jpg".to_owned(),
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
            max_consumer_data_bytes: 1024,
            max_image_pixels: 40_000_000,
            max_image_dimension: 12_000,
            variant_widths: vec![320, 640],
            magick_bin: "magick".to_owned(),
            magick_timeout: Duration::from_secs(45),
            image_processing_timeout: Duration::from_secs(120),
            trash_retention_days: 30,
        }
    }

    /// Which collection a content type belongs in. A client makes this choice;
    /// the URL is where the server learns about it.
    fn collection_for(content_type: &str) -> &'static str {
        if content_type.starts_with("image/") {
            "images"
        } else {
            "documents"
        }
    }

    fn asset_uri(collection: &str, id: &str) -> String {
        format!("/api/v1/{collection}/{id}")
    }

    fn upload_request(content_type: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/{}", collection_for(content_type)))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    fn pin_request(collection: &str, id: &str, pinned: bool) -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/{collection}/{id}/pin"))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"pinned":{pinned}}}"#)))
            .unwrap()
    }

    fn authorized(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer prototype-test-token")
            .body(Body::empty())
            .unwrap()
    }

    /// An authorized request with a JSON body, for the write endpoints.
    fn json_request(method: &str, uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap()
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        (status, value)
    }

    /// Upload a document and return its id. Documents live in their own
    /// collection, so every later call in a test says `/documents/...`.
    async fn upload_document(router: &Router, body: &[u8]) -> String {
        let (status, value) = send(router, upload_request("application/pdf", body.to_vec())).await;
        assert_eq!(status, StatusCode::CREATED);
        value["id"].as_str().unwrap().to_owned()
    }

    /// Upload an image into the images collection.
    async fn upload_image(router: &Router, body: &[u8]) -> String {
        let (status, value) = send(router, upload_request("image/png", body.to_vec())).await;
        assert_eq!(status, StatusCode::CREATED);
        value["id"].as_str().unwrap().to_owned()
    }

    fn upload_named(content_type: &str, body: &[u8], file_name: &str) -> Request<Body> {
        let mut request = upload_request(content_type, body.to_vec());
        let _ = &mut request;
        request.headers_mut().insert(
            ORIGINAL_FILENAME_HEADER,
            HeaderValue::from_bytes(file_name.as_bytes()).unwrap(),
        );
        request
    }

    /// Move an asset to the trash, through the collection it actually belongs to.
    async fn place_in_trash(router: &Router, collection: &str, id: &str) {
        let (status, _) = send(router, authorized("DELETE", &asset_uri(collection, id))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// A public media request, keeping the headers so a test can compare ETags.
    async fn fetch(router: &Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        fetch_with(router, uri, None).await
    }

    /// A public read with an `Accept` header, which is what a browser sends
    /// and what a crawler cannot.
    async fn fetch_with(
        router: &Router,
        uri: &str,
        accept: Option<&str>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder().uri(uri);
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    async fn fetch_error(router: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let (status, _, body) = fetch(router, uri).await;
        let value = serde_json::from_slice(&body).unwrap();
        (status, value)
    }

    fn write_test_image(path: &std::path::Path, size: &str) -> Vec<u8> {
        write_test_image_as(path, size, "PNG")
    }

    /// Bytes in the format they claim to be: Atlas checks the signature
    /// against the content type, so a test cannot hand it a PNG and say JPEG.
    fn write_test_image_as(path: &std::path::Path, size: &str, format: &str) -> Vec<u8> {
        let generated = Command::new("magick")
            .args(["-size", size, "xc:red"])
            .arg(format!("{format}:{}", path.display()))
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "could not generate a {size} test {format}"
        );
        std::fs::read(path).unwrap()
    }

    #[tokio::test]
    async fn delete_stops_serving_bytes_and_restore_brings_them_back() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let state = AppState::new(test_config(data_dir.clone())).await.unwrap();
        let router = app(state);

        let id = upload_document(&router, b"%PDF-1.7\ntrash me").await;
        let (status, body) = send(&router, authorized("GET", &asset_uri("documents", &id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "active");
        assert_eq!(body["pinned"], false);
        assert!(body["deleted_at"].is_null());

        place_in_trash(&router, "documents", &id).await;
        assert!(
            !data_dir.join("assets").join(&id).exists(),
            "the live directory is emptied by the delete"
        );
        assert!(data_dir.join("trash").join(&id).exists());

        let (status, body) = send(&router, authorized("GET", &asset_uri("documents", &id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "deleted");
        assert!(body["deleted_at"].is_i64());
        assert!(body["purged_at"].is_null());

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/original"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.headers()["cache-control"], "public, max-age=60");
        assert_eq!(
            response.headers()["content-length"],
            PLACEHOLDER_PNG.len().to_string()
        );
        let etag = response.headers()["etag"].clone();
        assert_eq!(
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap()[..],
            PLACEHOLDER_PNG[..]
        );

        let not_modified = router
            .clone()
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
        assert_eq!(
            not_modified.headers()["cache-control"],
            "public, max-age=60"
        );

        let (status, _) = send(&router, {
            let uri = asset_uri("documents", &id);
            authorized("POST", &format!("{uri}/restore"))
        })
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(data_dir.join("assets").join(&id).exists());
        assert!(!data_dir.join("trash").join(&id).exists());

        let restored = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/original"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(restored.status(), StatusCode::OK);
        assert_eq!(restored.headers()["content-type"], "application/pdf");
        assert_eq!(
            restored.headers()["cache-control"],
            "public, max-age=31536000, immutable"
        );

        let (status, body) = send(&router, authorized("GET", &asset_uri("documents", &id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "active");
        assert!(body["deleted_at"].is_null());
    }

    fn state_request(ids: &[&str]) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/assets/state")
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({ "ids": ids }).to_string()))
            .unwrap()
    }

    async fn states_of(router: &Router, ids: &[&str]) -> (StatusCode, serde_json::Value) {
        send(router, state_request(ids)).await
    }

    #[tokio::test]
    async fn a_collection_refuses_what_it_cannot_hold() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        // The classification is enforced, not trusted: a PDF posted to the
        // images collection is refused rather than stored as a picture.
        let (status, created) = send(
            &router,
            upload_request("application/pdf", b"%PDF-1.7\nwrong home".to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "documents accept it");
        assert_eq!(created["kind"], "document");
        let document = created["id"].as_str().unwrap().to_owned();

        let mut misfiled = upload_request("application/pdf", b"%PDF-1.7\nworse".to_vec());
        *misfiled.uri_mut() = "/api/v1/images".parse().unwrap();
        let (status, error) = send(&router, misfiled).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(error["error"]["code"], "unsupported_media_type");

        // ...and a document cannot be read through the images collection.
        for method in ["GET", "DELETE"] {
            let (status, _) =
                send(&router, authorized(method, &asset_uri("images", &document))).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{method} through the wrong collection"
            );
        }
        let (status, _) = send(
            &router,
            authorized(
                "POST",
                &format!("{}/restore", asset_uri("images", &document)),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Public delivery is addressed by id, so it does not care.
        let (status, _, _) = fetch(&router, &format!("/media/{document}/original")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_document_collection_stores_an_image_without_converting_it() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("diagram.png");
        let image = write_test_image(&source, "800x600");
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());

        let mut request = upload_request("image/png", image);
        *request.uri_mut() = "/api/v1/documents".parse().unwrap();
        let (status, body) = send(&router, request).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["kind"], "document");
        assert_eq!(
            body["renditions"].as_array().unwrap().len(),
            0,
            "an image stored as a document is not converted"
        );
        let id = body["id"].as_str().unwrap().to_owned();

        let stored = std::fs::read_dir(data_dir.join("assets").join(&id))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(stored, ["original.png".to_owned()].into());

        // It is served as a document: an attachment, not an inline picture.
        let (status, headers, _) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment")
        );

        // And there is no rendition route for documents at all.
        let (status, _) = send(
            &router,
            authorized("GET", &format!("/api/v1/documents/{id}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn identical_bytes_do_not_leak_across_collections() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("both.png");
        let image = write_test_image(&source, "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, photo) = send(&router, upload_request("image/png", image.clone())).await;
        assert_eq!(status, StatusCode::CREATED);
        let photo_id = photo["id"].as_str().unwrap().to_owned();
        assert_eq!(photo["renditions"].as_array().unwrap().len(), 2);

        // The same bytes as a document is a different asset, not the photo.
        let mut request = upload_request("image/png", image);
        *request.uri_mut() = "/api/v1/documents".parse().unwrap();
        let (status, document) = send(&router, request).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_ne!(document["id"], photo_id.as_str());
        assert_eq!(document["renditions"].as_array().unwrap().len(), 0);

        // And uploading it again as a document deduplicates against the
        // document, never against the photo.
        let mut again = upload_request("image/png", std::fs::read(&source).unwrap());
        *again.uri_mut() = "/api/v1/documents".parse().unwrap();
        let (status, repeated) = send(&router, again).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(repeated["id"], document["id"]);
        assert_eq!(repeated["deduplicated"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn one_call_reports_the_state_of_every_id_it_is_given() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let live = upload_document(&router, b"%PDF-1.7\nlive").await;
        let deleted = upload_document(&router, b"%PDF-1.7\ndeleted").await;
        let purged = upload_document(&router, b"%PDF-1.7\npurged").await;
        place_in_trash(&router, "documents", &deleted).await;
        place_in_trash(&router, "documents", &purged).await;
        let (status, _) = send(&router, pin_request("documents", &deleted, true)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) =
            states_of(&router, &[&live, &deleted, &purged, "aaaaaaaaaaaaaaaaaaaa"]).await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["assets"].as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["id"], live.as_str());
        assert_eq!(entries[0]["state"], "active");
        assert_eq!(entries[0]["pinned"], false);
        assert_eq!(entries[1]["state"], "deleted");
        assert_eq!(entries[1]["pinned"], true);
        assert_eq!(
            entries[2]["state"], "deleted",
            "purged is still deleted for now"
        );
        assert_eq!(
            entries[3]["state"], "missing",
            "an id Atlas never had is reported, not omitted"
        );
    }

    #[tokio::test]
    async fn a_state_lookup_refuses_malformed_ids_and_enormous_batches() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        for bad in ["../../etc/passwd", "UPPERCASEID", "short", "with space"] {
            let (status, error) = states_of(&router, &[bad]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(error["error"]["code"], "invalid_asset_ids", "{bad}");
        }

        let many: Vec<String> = (0..501).map(|index| format!("{index:018}")).collect();
        let borrowed: Vec<&str> = many.iter().map(String::as_str).collect();
        let (status, error) = states_of(&router, &borrowed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "too_many_ids");

        let (status, body) = states_of(&router, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["assets"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_state_lookup_needs_the_token() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/assets/state")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ids":["aaaaaaaaaaaaaaaaaaaa"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_photo_smaller_than_every_profile_still_answers_each_one() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("small.png"), "200x150");
        let data_dir = directory.path().join("atlas");

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![320, 640, 960];
        let router = app(AppState::new(config).await.unwrap());

        let (status, body) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().unwrap().to_owned();

        // 640 and 960 collapse into the same actual width as 320, so exactly
        // one rendition is generated and it is named for the real width.
        let renditions = body["renditions"].as_array().unwrap();
        assert_eq!(renditions.len(), 1);
        assert_eq!(renditions[0]["max_width"], 320);
        assert_eq!(
            renditions[0]["width"], 200,
            "Atlas reports the actual output width, not the profile"
        );
        assert!(
            data_dir.join("assets").join(&id).join("w200.png").exists(),
            "the stored file is named after the actual width"
        );

        // Every profile resolves, including the two generation deduplicated.
        let (status, _, bytes) = fetch(&router, &format!("/media/{id}/w320")).await;
        assert_eq!(status, StatusCode::OK);
        let expected = bytes.len();
        assert!(expected > 0);
        for requested in [640, 960, 5000] {
            let (status, _, body) = fetch(&router, &format!("/media/{id}/w{requested}")).await;
            assert_eq!(status, StatusCode::OK, "w{requested}");
            assert_eq!(
                body.len(),
                expected,
                "w{requested} serves the one rendition that exists"
            );
        }

        // Nothing is narrower than the stored rendition, and Atlas never
        // upscales, so this asks for something that was never generated.
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w160")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "rendition_not_found");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stored widths are 320"),
            "{}",
            error["error"]["message"]
        );

        // A missing asset is a different answer to the same status code.
        let (status, error) = fetch_error(&router, "/media/aaaaaaaaaaaaaaaaaaaa/w320").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "asset_not_found");

        // The original is untouched and still served.
        let (status, headers, body) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/png");
        assert!(
            body.len() > expected,
            "the original is larger than its rendition"
        );
    }

    /// The colour of the first pixel of a PNG, as `[r, g, b]`.
    fn first_pixel(path: &std::path::Path) -> [i32; 3] {
        let output = Command::new("magick")
            .arg(format!("PNG:{}", path.display()))
            .args(["-format", "%[pixel:p{0,0}]", "info:"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let pixel = String::from_utf8(output.stdout).unwrap();
        let channels: Vec<i32> = pixel
            .trim()
            .trim_start_matches("srgb(")
            .trim_end_matches(')')
            .split(',')
            .map(|channel| channel.parse().unwrap())
            .collect();
        channels.try_into().unwrap()
    }

    #[tokio::test]
    async fn a_display_p3_photo_is_converted_to_srgb_rather_than_relabelled() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());
        let p3 = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/DisplayP3-v4.icc"
        );

        // The same numbers twice: once meaning Display P3, once meaning sRGB.
        let mut stored = Vec::new();
        for (name, profile) in [("p3.png", Some(p3)), ("untagged.png", None)] {
            let path = directory.path().join(name);
            let mut command = Command::new("magick");
            command.args(["-size", "40x30", "xc:rgb(200,100,50)"]);
            if let Some(profile) = profile {
                command.args(["-profile", &format!("ICC:{profile}")]);
            }
            let generated = command
                .arg(format!("PNG:{}", path.display()))
                .output()
                .unwrap();
            assert!(generated.status.success(), "could not generate {name}");

            let (status, body) = send(
                &router,
                upload_request("image/png", std::fs::read(&path).unwrap()),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            let id = body["id"].as_str().unwrap();
            stored.push(first_pixel(
                &data_dir.join("assets").join(id).join("w40.png"),
            ));
        }

        // P3 rgb(200,100,50) in sRGB, from the published primaries. Relabelling
        // instead of converting keeps the numbers and loses the saturation.
        let [converted, untagged] = stored.try_into().unwrap();
        for (channel, wanted) in converted.into_iter().zip([214, 92, 30]) {
            assert!(
                (channel - wanted).abs() <= 2,
                "{converted:?} is not the sRGB of P3 rgb(200,100,50)"
            );
        }
        assert_eq!(
            untagged,
            [200, 100, 50],
            "an untagged photo keeps its pixels"
        );
    }

    #[tokio::test]
    async fn a_request_between_profiles_gets_the_nearest_width_at_or_below() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("wide.png"), "1600x1200");
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320, 640, 960];
        let router = app(AppState::new(config).await.unwrap());

        let (status, body) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().unwrap().to_owned();
        assert_eq!(body["renditions"].as_array().unwrap().len(), 3);

        let mut etags = Vec::new();
        for width in [320, 640, 960] {
            let (status, headers, _) = fetch(&router, &format!("/media/{id}/w{width}")).await;
            assert_eq!(status, StatusCode::OK, "w{width}");
            etags.push(headers["etag"].to_str().unwrap().to_owned());
        }
        assert_eq!(
            etags.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "each profile serves its own rendition"
        );

        // A width that is not a profile resolves to the widest stored
        // rendition that is not wider than the request.
        let (_, headers, _) = fetch(&router, &format!("/media/{id}/w800")).await;
        assert_eq!(headers["etag"], etags[1], "w800 resolves to w640");
        let (status, headers, _) = fetch(&router, &format!("/media/{id}/w3000")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["etag"], etags[2], "w3000 resolves to w960");
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w100")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "rendition_not_found");
    }

    #[tokio::test]
    async fn identical_bytes_come_back_as_the_asset_that_already_exists() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());

        let body = b"%PDF-1.7\nthe very same document".to_vec();
        let (status, created) =
            send(&router, upload_request("application/pdf", body.clone())).await;
        assert_eq!(status, StatusCode::CREATED, "the first upload creates");
        let id = created["id"].as_str().unwrap().to_owned();

        let (status, repeated) = send(&router, upload_request("application/pdf", body)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a repeat is not a creation, and the status says so"
        );
        assert_eq!(repeated["id"], id.as_str(), "the same ID comes back");
        assert_eq!(repeated["created_at"], created["created_at"]);

        let mut directories = std::fs::read_dir(data_dir.join("assets")).unwrap();
        assert!(directories.next().is_some());
        assert!(
            directories.next().is_none(),
            "one directory: no second copy of the file and its renditions"
        );
        let staged = std::fs::read_dir(data_dir.join("staging"))
            .unwrap()
            .next()
            .is_some();
        assert!(!staged, "the staged copy of a duplicate is not left behind");
    }

    #[tokio::test]
    async fn a_deduplicated_upload_costs_no_image_conversion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("repeated.png");
        let image = write_test_image(&source, "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, created) = send(&router, upload_request("image/png", image.clone())).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_owned();
        let data_dir = directory.path().join("atlas");
        let stored = std::fs::read_dir(data_dir.join("assets").join(&id))
            .unwrap()
            .count();
        // The test configuration generates two widths: original plus 2x4 formats.
        assert_eq!(stored, 9, "the original plus four formats at two widths");

        let (status, body) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id.as_str());
        assert_eq!(
            std::fs::read_dir(data_dir.join("assets").join(&id))
                .unwrap()
                .count(),
            stored,
            "nothing was written a second time"
        );
    }

    #[tokio::test]
    async fn a_deleted_asset_does_not_shadow_a_new_upload_of_the_same_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let body = b"%PDF-1.7\ndeleted then uploaded again".to_vec();
        let (status, first) = send(&router, upload_request("application/pdf", body.clone())).await;
        assert_eq!(status, StatusCode::CREATED);
        let first_id = first["id"].as_str().unwrap().to_owned();
        place_in_trash(&router, "documents", &first_id).await;

        let (status, second) = send(&router, upload_request("application/pdf", body)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "re-uploading after a delete adds the file again"
        );
        assert_ne!(second["id"], first_id.as_str());
    }

    #[tokio::test]
    async fn a_pinned_or_trashed_twin_does_not_change_a_new_upload() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        // Different bytes, so nothing here is deduplicated: the point is that a
        // normal upload is untouched by the lookup.
        for index in 0..3 {
            let (status, _) = send(
                &router,
                upload_request(
                    "application/pdf",
                    format!("%PDF-1.7\ndistinct {index}").into_bytes(),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "upload {index}");
        }
    }

    async fn listing(router: &Router, query: &str) -> serde_json::Value {
        let (status, body) = send(
            router,
            authorized("GET", &format!("/api/v1/images?{query}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query} answered {body}");
        body
    }

    fn ids_of(page: &serde_json::Value) -> Vec<String> {
        page["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The kind a listing filter is asking about, for a second collection.
    async fn document_listing(router: &Router, query: &str) -> serde_json::Value {
        let (status, body) = send(
            router,
            authorized("GET", &format!("/api/v1/documents?{query}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}");
        body
    }

    /// Media type, not kind: the documents collection takes a picture too, and
    /// telling "the invoices" from "the diagrams" is the question.
    /// What an administrator can upload is what Atlas can produce.
    ///
    /// Before this, Atlas served AVIF and JPEG XL renditions while refusing
    /// both on upload, so the file you had just downloaded could not be put
    /// back. HEIC is here for the same practical reason: it is what a phone
    /// hands you.
    #[tokio::test]
    async fn every_format_atlas_produces_can_also_be_uploaded() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config).await.unwrap());

        for (content_type, magick) in [
            ("image/avif", "AVIF"),
            ("image/jxl", "JXL"),
            ("image/heic", "HEIC"),
        ] {
            let extension = content_type.rsplit('/').next().unwrap();
            let bytes = write_test_image_as(
                &directory.path().join(format!("src.{extension}")),
                "100x80",
                magick,
            );
            let (status, asset) = send(&router, upload_named(content_type, &bytes, "photo")).await;
            assert_eq!(status, StatusCode::CREATED, "{content_type}");
            assert_eq!(asset["kind"], "image", "{content_type}");
            assert_eq!(
                asset["original"]["content_type"], content_type,
                "stored as what it claimed to be"
            );
            assert!(
                !asset["renditions"].as_array().unwrap().is_empty(),
                "{content_type} should still get renditions"
            );
        }
    }

    /// The round trip that motivated the above, in the direction that used to
    /// break: read a rendition off disk and offer it straight back.
    #[tokio::test]
    async fn a_rendition_can_be_uploaded_again() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let state = AppState::new(config).await.unwrap();
        let data_dir = state.config.data_dir.clone();
        let router = app(state);

        let image = write_test_image(&directory.path().join("source.png"), "400x300");
        let (status, asset) = send(&router, upload_named("image/png", &image, "photo.png")).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = asset["id"].as_str().unwrap().to_owned();

        for extension in ["jxl", "avif", "webp"] {
            let rendition = std::fs::read(
                data_dir
                    .join("assets")
                    .join(&id)
                    .join(format!("w320.{extension}")),
            )
            .unwrap_or_else(|error| panic!("the {extension} rendition should exist: {error}"));
            let content_type = format!("image/{extension}");
            let (status, again) =
                send(&router, upload_named(&content_type, &rendition, "again")).await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "a {extension} rendition should be uploadable again"
            );
            assert_eq!(again["kind"], "image", "{extension}");
        }
    }

    /// A picture can be stored whole, whatever format it is in.
    #[tokio::test]
    async fn a_heic_photo_can_be_stored_as_a_document_without_conversion() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let heic = write_test_image_as(&directory.path().join("diagram.heic"), "120x90", "HEIC");
        let (status, asset) = send(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/documents")
                .header("authorization", "Bearer prototype-test-token")
                .header("content-type", "image/heic")
                .body(Body::from(heic.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(asset["kind"], "document");
        assert_eq!(asset["original"]["content_type"], "image/heic");
        assert_eq!(
            asset["renditions"].as_array().unwrap().len(),
            0,
            "a document is stored whole"
        );
    }

    /// Declaring one format and sending another is still refused, including
    /// between the two formats that share an `ftyp` box.
    #[tokio::test]
    async fn bytes_must_match_the_format_they_are_declared_as() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let avif = write_test_image_as(&directory.path().join("real.avif"), "60x40", "AVIF");
        let heic = write_test_image_as(&directory.path().join("real.heic"), "60x40", "HEIC");

        let (status, error) =
            send(&router, upload_named("image/heic", &avif, "disguised.heic")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("do not match the declared media type"),
            "the answer should say what is wrong, said: {error}"
        );

        let (status, _) = send(&router, upload_named("image/avif", &heic, "disguised.avif")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // And the ordinary mismatch, unchanged.
        let png = write_test_image(&directory.path().join("real.png"), "60x40");
        let (status, _) = send(&router, upload_named("image/jxl", &png, "disguised.jxl")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// The other spelling of HEIF.
    ///
    /// freedesktop.org makes `image/heif` the canonical type for a `.heic`
    /// file and keeps `image/heic` as its alias, so which one a browser sends
    /// is the desktop's decision. A file manager that answers `image/heif` for
    /// a HEIC photograph used to be told the format was not accepted.
    #[tokio::test]
    async fn heic_may_be_declared_as_image_heif() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config).await.unwrap());

        let heic = write_test_image_as(&directory.path().join("photo.heic"), "120x90", "HEIC");
        let (status, asset) = send(&router, upload_named("image/heif", &heic, "photo.heic")).await;
        assert_eq!(status, StatusCode::CREATED, "declared as image/heif");
        assert_eq!(asset["kind"], "image");
        assert_eq!(
            asset["original"]["content_type"], "image/heic",
            "stored under the canonical spelling whatever it was declared as"
        );
        assert_eq!(asset["original"]["width"], 120);
        assert_eq!(asset["original"]["height"], 90);
        assert!(
            !asset["renditions"].as_array().unwrap().is_empty(),
            "image/heif is an image, so it gets renditions"
        );
    }

    /// The alias opens a declaration, not a door.
    ///
    /// `image/heif` names a container family whose payload may be AV1, and
    /// Atlas decodes HEVC. An AV1 file wearing the other spelling is still
    /// refused, because the bytes are what decide.
    #[tokio::test]
    async fn the_heif_spelling_does_not_admit_an_av1_file() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let avif = write_test_image_as(&directory.path().join("real.avif"), "60x40", "AVIF");
        let (status, error) =
            send(&router, upload_named("image/heif", &avif, "disguised.heif")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("do not match the declared media type"),
            "said: {error}"
        );
    }

    /// The image sequences are not pictures.
    ///
    /// `image/heic-sequence` is a grid of images in one container, not one
    /// image, and ImageMagick would silently keep only the first frame. It is
    /// not in the allowlist and adding it would buy nothing.
    #[tokio::test]
    async fn an_image_sequence_is_not_an_image() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let heic = write_test_image_as(&directory.path().join("photo.heic"), "60x40", "HEIC");
        for content_type in ["image/heic-sequence", "image/heif-sequence"] {
            let (status, _) = send(&router, upload_named(content_type, &heic, "photo")).await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type} is not a picture Atlas can hold"
            );
        }
    }

    /// A consumer building an upload form should be able to ask rather than
    /// hardcode a list that drifts on the next release.
    /// A picture stored as a document sits in the documents collection while
    /// its media type says "image", so a caller listing the whole trash cannot
    /// address its restore route from the format alone.
    #[tokio::test]
    async fn a_trash_entry_says_which_collection_it_lives_in() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let image = write_test_image(&directory.path().join("diagram.png"), "60x40");
        let (status, asset) = send(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/documents")
                .header("authorization", "Bearer prototype-test-token")
                .header("content-type", "image/png")
                .body(Body::from(image))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = asset["id"].as_str().unwrap().to_owned();
        assert_eq!(asset["kind"], "document");

        place_in_trash(&router, "documents", &id).await;

        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        let entry = body["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id.as_str())
            .expect("the asset should be in the trash listing");
        assert_eq!(
            entry["kind"], "document",
            "its format says image; where it lives is documents"
        );
        assert_eq!(entry["content_type"], "image/png");

        // And that is enough to address the restore route.
        let (status, _) = send(
            &router,
            authorized(
                "POST",
                &format!("/api/v1/{}/{id}/restore", entry["kind"].as_str().unwrap()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// A consumer's opaque note comes back exactly as written, and nothing on
    /// this path looks inside it. The value below is deliberately awkward:
    /// unicode, newlines, markup, and something that only *looks* like JSON.
    const OPAQUE: &str = "Прогулки\n<em>с Alder</em> — {\"looks\": \"like json\"}";

    async fn data_test_asset() -> (tempfile::TempDir, Router, String) {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        // Named, so the test can also show that searching the *filename* still
        // works while searching the note does not.
        let (status, body) = send(
            &router,
            upload_named("application/pdf", b"%PDF-1.7\nnotes", "holiday notes.pdf"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_owned();
        (directory, router, id)
    }

    async fn put_data(
        router: &Router,
        collection: &str,
        id: &str,
        data: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        send(
            router,
            json_request("PUT", &format!("/api/v1/{collection}/{id}/data"), &data),
        )
        .await
    }

    /// A call that is only interesting if it is refused.
    async fn put_data_expecting(
        router: &Router,
        collection: &str,
        id: &str,
        data: serde_json::Value,
        status: StatusCode,
    ) {
        let (got, body) = put_data(router, collection, id, data).await;
        assert_eq!(got, status, "{body}");
    }

    #[tokio::test]
    async fn a_consumers_data_comes_back_exactly_as_written() {
        let (_directory, router, id) = data_test_asset().await;

        let (status, _) = put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": OPAQUE }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = send(
            &router,
            authorized("GET", &format!("/api/v1/documents/{id}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"], OPAQUE, "byte for byte, not reformatted");

        // Replacing it replaces it; there is no merge.
        let (status, _) = put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": "just this" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = send(
            &router,
            authorized("GET", &format!("/api/v1/documents/{id}")),
        )
        .await;
        assert_eq!(body["data"], "just this");

        // And null clears it, which makes the field absent rather than empty.
        let (status, _) = put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = send(
            &router,
            authorized("GET", &format!("/api/v1/documents/{id}")),
        )
        .await;
        assert!(
            body.get("data").is_none(),
            "cleared, not set to an empty string: {body}"
        );
    }

    #[tokio::test]
    async fn a_listing_never_carries_a_consumers_data() {
        let (_directory, router, id) = data_test_asset().await;
        put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": OPAQUE }),
        )
        .await;

        let (status, body) = send(&router, authorized("GET", "/api/v1/documents")).await;
        assert_eq!(status, StatusCode::OK);
        let entry = body["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == id.as_str())
            .expect("the asset should be in the page");
        assert!(
            entry.get("data").is_none(),
            "a page of assets must not grow into a page of blobs: {entry}"
        );
    }

    /// The invariant that makes the field safe to have: Atlas does not read it.
    #[tokio::test]
    async fn a_consumers_data_is_never_searched() {
        let (_directory, router, id) = data_test_asset().await;
        put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": "zzzunmistakablezzz" }),
        )
        .await;

        let (_, by_name) = send(&router, authorized("GET", "/api/v1/documents?q=notes")).await;
        assert_eq!(
            by_name["total"], 1,
            "the filename still matches, as it always did"
        );

        let (_, by_data) = send(
            &router,
            authorized("GET", "/api/v1/documents?q=zzzunmistakablezzz"),
        )
        .await;
        assert_eq!(
            by_data["total"], 0,
            "Atlas does not index what it does not own: searching it would mean interpreting it"
        );
    }

    #[tokio::test]
    async fn consumers_data_is_capped_and_addressed_like_everything_else() {
        let (_directory, router, id) = data_test_asset().await;

        // The test config caps data at 1 KiB.
        let big = "x".repeat(2048);
        let (status, error) = put_data(
            &router,
            "documents",
            &id,
            serde_json::json!({ "data": big }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "data_too_large");

        // The note that fit is still the one that is stored: a refused write
        // changed nothing.
        let (_, body) = send(
            &router,
            authorized("GET", &format!("/api/v1/documents/{id}")),
        )
        .await;
        assert!(body.get("data").is_none());

        put_data_expecting(
            &router,
            "images",
            &id,
            serde_json::json!({ "data": "wrong collection" }),
            StatusCode::NOT_FOUND,
        )
        .await;
        put_data_expecting(
            &router,
            "documents",
            "nosuchid0000000000",
            serde_json::json!({ "data": "unknown id" }),
            StatusCode::NOT_FOUND,
        )
        .await;

        let (status, _) = send(
            &router,
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/documents/{id}/data"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "data": "no token" })).unwrap(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_accepted_formats_are_readable_and_true() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, body) = send(&router, authorized("GET", "/api/v1/formats")).await;
        assert_eq!(status, StatusCode::OK);
        // The format list holds one media type per format: HEIF has two
        // spellings, and a consumer counting this list counts formats.
        let images = body["images"]["formats"].as_array().unwrap();
        assert_eq!(
            images,
            &vec![
                json!("image/jpeg"),
                json!("image/png"),
                json!("image/webp"),
                json!("image/avif"),
                json!("image/jxl"),
                json!("image/heic"),
            ]
        );
        // The media type list holds every spelling a caller may declare.
        let spellings = body["images"]["media_types"].as_array().unwrap();
        assert_eq!(
            spellings,
            &vec![
                json!("image/jpeg"),
                json!("image/png"),
                json!("image/webp"),
                json!("image/avif"),
                json!("image/jxl"),
                json!("image/heic"),
                json!("image/heif"),
            ]
        );
        assert_eq!(body["images"]["groups"], json!(["image/*"]));
        assert_eq!(body["images"]["generates_renditions"], json!(true));

        // Documents take everything, which is the point of the collection.
        let documents = body["documents"]["formats"].as_array().unwrap();
        assert_eq!(documents.len(), SourceFormat::ALL.len());
        assert_eq!(
            body["documents"]["media_types"].as_array().unwrap().len(),
            SourceFormat::ALL
                .iter()
                .map(|format| format.accepted_content_types().len())
                .sum::<usize>()
        );
        assert_eq!(
            body["documents"]["groups"],
            json!(["application/*", "image/*"])
        );
        assert_eq!(body["documents"]["generates_renditions"], json!(false));

        // Nothing listed may be refused by the endpoint that validates it, so
        // each advertised type is offered bytes that really are that type.
        // The spellings are walked, not the formats: `image/heif` is a
        // spelling Atlas accepts, so it has to be proven accepted too.
        // The sizes differ because two spellings of one format are the same
        // bytes, and identical bytes are one asset: the second upload would
        // be deduplicated and answer 200 rather than 201.
        for (index, content_type) in spellings.iter().enumerate() {
            let content_type = content_type.as_str().unwrap();
            let format = SourceFormat::parse_content_type(content_type).unwrap();
            let side = 20 + index;
            let bytes = write_test_image_as(
                &directory
                    .path()
                    .join(format!("probe{index}.{}", format.extension())),
                &format!("{side}x{side}"),
                format.magick_name().unwrap(),
            );
            let (status, error) = send(&router, upload_named(content_type, &bytes, "probe")).await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "{content_type} is advertised but refused: {error}"
            );
        }

        let (status, _) = send(&router, authorized("GET", "/api/v1/formats")).await;
        assert_eq!(status, StatusCode::OK);
        let anonymous = send(
            &router,
            Request::builder()
                .uri("/api/v1/formats")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(anonymous.0, StatusCode::UNAUTHORIZED);
    }

    /// The two lists answer two questions, and a consumer that reads one as
    /// the other gets a wrong answer rather than an error.
    ///
    /// Counting the format list counts formats; counting the media type list
    /// counts spellings, and HEIF has two. Every format appears once in the
    /// first list and every spelling of it in the second.
    #[tokio::test]
    async fn the_format_list_counts_formats_and_the_media_type_list_counts_spellings() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, body) = send(&router, authorized("GET", "/api/v1/formats")).await;
        assert_eq!(status, StatusCode::OK);

        for collection in ["images", "documents"] {
            let formats = body[collection]["formats"].as_array().unwrap();
            let media_types = body[collection]["media_types"].as_array().unwrap();

            // One entry per format, and no format listed twice.
            let mut unique = formats.clone();
            unique.sort_by(|a, b| a.as_str().unwrap().cmp(b.as_str().unwrap()));
            unique.dedup();
            assert_eq!(
                unique.len(),
                formats.len(),
                "{collection} lists a format twice"
            );

            // Every canonical type is also an accepted type.
            for format in formats {
                assert!(
                    media_types.contains(format),
                    "{collection} lists {format} but does not accept it"
                );
            }

            // The spellings are the formats' own, so the two lists cannot
            // drift apart: the media type list is the format list with the
            // alternative spellings put back in.
            let expected: Vec<&str> = formats
                .iter()
                .map(|format| format.as_str().unwrap())
                .flat_map(|canonical| {
                    SourceFormat::parse_content_type(canonical)
                        .unwrap()
                        .accepted_content_types()
                        .iter()
                        .copied()
                })
                .collect();
            let listed: Vec<&str> = media_types
                .iter()
                .map(|media_type| media_type.as_str().unwrap())
                .collect();
            assert_eq!(listed, expected, "{collection} lists other spellings");
        }
    }

    #[tokio::test]
    async fn a_listing_can_filter_by_media_type() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config).await.unwrap());

        let png = write_test_image(&directory.path().join("one.png"), "40x30");
        let (status, value) = send(&router, upload_named("image/png", &png, "one.png")).await;
        assert_eq!(status, StatusCode::CREATED);
        let png_id = value["id"].as_str().unwrap().to_owned();

        let jpeg = write_test_image_as(&directory.path().join("two.jpg"), "60x45", "JPEG");
        let (status, value) = send(&router, upload_named("image/jpeg", &jpeg, "two.jpg")).await;
        assert_eq!(status, StatusCode::CREATED);
        let jpeg_id = value["id"].as_str().unwrap().to_owned();

        let (status, _) = send(
            &router,
            upload_named("application/pdf", b"%PDF-1.7\ninvoice", "invoice.pdf"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // The same picture, stored whole as a document: the reason the filter
        // has to work across both collections.
        let diagram = write_test_image(&directory.path().join("diagram.png"), "80x60");
        let (status, value) = send(
            &router,
            Request::builder()
                .method("POST")
                .uri("/api/v1/documents")
                .header("authorization", "Bearer prototype-test-token")
                .header("content-type", "image/png")
                .body(Body::from(diagram))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let diagram_id = value["id"].as_str().unwrap().to_owned();

        assert_eq!(
            ids_of(&listing(&router, "mime=image/png").await),
            vec![png_id.clone()],
            "an exact type is exact"
        );
        let mut grouped = ids_of(&listing(&router, "mime=image%2F%2A").await);
        grouped.sort();
        let mut expected = vec![png_id, jpeg_id];
        expected.sort();
        assert_eq!(grouped, expected, "a group is every type in it");

        let mut stored = ids_of(&document_listing(&router, "mime=image%2F%2A").await);
        stored.sort();
        assert_eq!(
            stored,
            vec![diagram_id],
            "documents can hold a picture, and this is how you find it"
        );
        assert_eq!(
            ids_of(&document_listing(&router, "mime=application%2Fpdf").await).len(),
            1,
            "and the invoice is still findable by its own type"
        );

        // Either spelling of HEIF finds the same picture.
        let heic = write_test_image_as(&directory.path().join("wedding.heic"), "70x50", "HEIC");
        let (status, value) =
            send(&router, upload_named("image/heif", &heic, "wedding.heic")).await;
        assert_eq!(status, StatusCode::CREATED);
        let heic_id = value["id"].as_str().unwrap().to_owned();
        for query in ["mime=image%2Fheif", "mime=image%2Fheic"] {
            assert_eq!(
                ids_of(&listing(&router, query).await),
                vec![heic_id.clone()],
                "{query} finds the HEIC picture"
            );
        }
    }

    #[tokio::test]
    async fn a_media_type_that_cannot_match_is_refused_rather_than_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        for (query, collection, expected) in [
            ("mime=image%2Ftiff", "images", "such as image/png"),
            ("mime=text%2F%2A", "documents", "a group such as image/*"),
            ("mime=image%2F%2A%2Fx", "images", "such as image/png"),
        ] {
            let (status, error) = send(
                &router,
                authorized("GET", &format!("/api/v1/{collection}?{query}")),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(error["error"]["code"], "invalid_mime", "{query}");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{query} should point at the formats listing, said: {error}"
            );
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("/api/v1/formats"),
                "{query} should say where the list is, said: {error}"
            );
        }

        // A real type that this collection cannot hold is the caller's bug,
        // and the answer says which collection was meant.
        let (status, error) = send(
            &router,
            authorized("GET", "/api/v1/images?mime=application%2Fpdf"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_mime");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("cannot match the images collection"),
            "said: {message}"
        );
        assert!(
            message.contains("/api/v1/formats"),
            "and where to find what it holds, said: {message}"
        );

        // The mirror image is fine: a picture stored as a document is real.
        let (status, _) = send(
            &router,
            authorized("GET", "/api/v1/documents?mime=image%2Fjpeg"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_listing_page_filters_sorts_and_counts() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config.clone()).await.unwrap());

        let mut expected = Vec::new();
        for (name, size) in [
            ("alpha.png", "40x30"),
            ("beta.png", "120x90"),
            ("gamma.png", "60x45"),
        ] {
            let image = write_test_image(&directory.path().join(name), size);
            let (status, value) = send(&router, upload_named("image/png", &image, name)).await;
            assert_eq!(status, StatusCode::CREATED);
            expected.push(value["id"].as_str().unwrap().to_owned());
        }
        // A document is not in the images listing at all.
        let (status, _) = send(
            &router,
            upload_request("application/pdf", b"%PDF-1.7\nnot listed".to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let page = listing(&router, "").await;
        assert_eq!(page["total"], 3);
        assert_eq!(page["limit"], 50);
        assert_eq!(page["offset"], 0);
        assert_eq!(page["assets"].as_array().unwrap()[0]["kind"], "image");
        assert_eq!(
            page["assets"][0]["renditions"].as_array().unwrap().len(),
            1,
            "a listing carries the widths a grid needs for srcset"
        );

        // Name search, with the LIKE wildcards treated literally.
        assert_eq!(
            ids_of(&listing(&router, "q=bet").await),
            vec![expected[1].clone()]
        );
        assert!(
            ids_of(&listing(&router, "q=%25").await).is_empty(),
            "% is literal"
        );

        // Size and dimension filters.
        let wide = ids_of(&listing(&router, "width_min=100").await);
        assert_eq!(wide, vec![expected[1].clone()]);

        // Sorting, and a stable tiebreaker so pages cannot overlap or skip.
        let by_name = ids_of(&listing(&router, "sort=filename&order=asc&limit=2").await);
        assert_eq!(by_name, vec![expected[0].clone(), expected[1].clone()]);
        let second = ids_of(&listing(&router, "sort=filename&order=asc&limit=2&offset=2").await);
        assert_eq!(second, vec![expected[2].clone()]);

        // Junk is refused rather than silently ignored.
        for (query, code) in [
            ("sort=colour", "invalid_sort"),
            ("order=sideways", "invalid_order"),
            ("state=elsewhere", "invalid_state"),
        ] {
            let (status, error) = send(
                &router,
                authorized("GET", &format!("/api/v1/images?{query}")),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(error["error"]["code"], code, "{query}");
        }

        // An unknown collection is not a route.
        let (status, _) = send(&router, authorized("GET", "/api/v1/pictures")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_listing_can_see_retired_assets_or_only_live_ones() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        let id = upload_document(&router, b"%PDF-1.7\nlisted").await;

        assert_eq!(
            listing(&router, "").await["total"],
            0,
            "documents are not in the images listing"
        );
        let (status, documents) = send(&router, authorized("GET", "/api/v1/documents")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(documents["total"], 1);

        place_in_trash(&router, "documents", &id).await;
        let (_, live) = send(&router, authorized("GET", "/api/v1/documents")).await;
        assert_eq!(live["total"], 0, "the default view is live assets");
        let (_, all) = send(&router, authorized("GET", "/api/v1/documents?state=all")).await;
        assert_eq!(all["total"], 1);
        assert_eq!(all["assets"][0]["state"], "deleted");
        let (_, retired) = send(
            &router,
            authorized("GET", "/api/v1/documents?state=deleted"),
        )
        .await;
        assert_eq!(retired["total"], 1);
        let (_, purged) = send(&router, authorized("GET", "/api/v1/documents?state=purged")).await;
        assert_eq!(
            purged["total"], 0,
            "still in the trash is not purged: a restore would work"
        );
    }

    /// `state=deleted` and `state=purged` are different questions, and a
    /// consumer asking the second one must not get the first one's answer.
    #[tokio::test]
    async fn a_listing_separates_the_trash_from_the_tombstones() {
        let directory = tempfile::tempdir().unwrap();
        let config = test_config(directory.path().join("atlas"));
        let state = AppState::new(config.clone()).await.unwrap();
        let expired = upload_document(&app(state.clone()), b"%PDF-1.7\nexpired").await;
        let recent = upload_document(&app(state.clone()), b"%PDF-1.7\trecent").await;
        let router = app(state.clone());
        place_in_trash(&router, "documents", &expired).await;
        place_in_trash(&router, "documents", &recent).await;

        // Age one of them past the retention window and restart, which is when
        // purge runs.
        let long_ago = crate::unix_now().unwrap() - 40 * 24 * 60 * 60;
        state
            .store
            .backdate_deletion(&AssetId::parse(&expired).unwrap(), long_ago)
            .await;
        drop(state);
        drop(router);

        let state = AppState::new(config).await.unwrap();
        let router = app(state);

        let (_, deleted) = send(
            &router,
            authorized("GET", "/api/v1/documents?state=deleted"),
        )
        .await;
        assert_eq!(deleted["total"], 1);
        assert_eq!(
            deleted["assets"][0]["id"], recent,
            "the one that can still be restored"
        );

        let (_, purged) = send(&router, authorized("GET", "/api/v1/documents?state=purged")).await;
        assert_eq!(purged["total"], 1);
        assert_eq!(purged["assets"][0]["id"], expired);

        let (_, all) = send(&router, authorized("GET", "/api/v1/documents?state=all")).await;
        assert_eq!(all["total"], 2, "and `all` still means every state");
    }

    #[tokio::test]
    async fn a_listing_needs_the_token() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_summary_counts_each_collection() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config).await.unwrap());

        let image = write_test_image(&directory.path().join("one.png"), "40x30");
        upload_image(&router, &image).await;
        upload_document(&router, b"%PDF-1.7\none").await;
        upload_document(&router, b"%PDF-1.7\ntwo").await;

        let (status, body) = send(&router, authorized("GET", "/api/v1/summary")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["images"]["count"], 1);
        assert_eq!(body["documents"]["count"], 2);
        assert_eq!(body["total"]["count"], 3);
        assert!(body["total"]["bytes"].as_i64().unwrap() > 0);
    }

    fn action_request(collection: &str, action: &str, ids: &[&str]) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/{collection}/actions"))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "action": action, "ids": ids }).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn a_bulk_action_applies_to_every_asset_in_the_batch() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let state = AppState::new(config.clone()).await.unwrap();
        let router = app(state.clone());

        let mut ids = Vec::new();
        for index in 0..3 {
            // Distinct bytes: identical ones would deduplicate into one asset.
            let image = write_test_image(
                &directory.path().join(format!("bulk{index}.png")),
                &format!("{}x30", 40 + index),
            );
            ids.push(upload_image(&router, &image).await);
        }
        let borrowed: Vec<&str> = ids.iter().map(String::as_str).collect();
        drop(router);
        drop(state);

        let router = app(AppState::new(config.clone()).await.unwrap());
        let (status, body) = send(&router, action_request("images", "delete", &borrowed)).await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|entry| entry["status"] == "deleted"));
        for id in &ids {
            assert!(
                directory
                    .path()
                    .join("atlas")
                    .join("trash")
                    .join(id)
                    .exists()
            );
        }

        let (status, _) = send(&router, action_request("images", "restore", &borrowed)).await;
        assert_eq!(status, StatusCode::OK);
        for id in &ids {
            assert!(
                directory
                    .path()
                    .join("atlas")
                    .join("assets")
                    .join(id)
                    .exists()
            );
        }

        let (status, body) = send(&router, action_request("images", "pin", &borrowed)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["status"] == "pinned")
        );
        let (_, listing) = send(&router, authorized("GET", "/api/v1/images")).await;
        assert_eq!(listing["assets"][0]["pinned"], true);
    }

    #[tokio::test]
    async fn a_bulk_batch_is_all_or_nothing_before_it_starts() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.variant_widths = vec![320];
        let router = app(AppState::new(config.clone()).await.unwrap());

        let image = write_test_image(&directory.path().join("one.png"), "40x30");
        let id = upload_image(&router, &image).await;
        let document = upload_document(&router, b"%PDF-1.7\nother collection").await;

        // A typo in one id takes the whole batch, before anything is deleted.
        let (status, error) = send(
            &router,
            action_request("images", "delete", &[&id, "aaaaaaaaaaaaaaaaaaaa"]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "unknown_asset");
        assert!(
            directory
                .path()
                .join("atlas")
                .join("assets")
                .join(&id)
                .exists()
        );

        // So does an asset that exists but is in the other collection.
        let (status, error) = send(
            &router,
            action_request("images", "delete", &[&id, &document]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "unknown_asset");

        for (action, ids, code) in [
            ("delete", ["not-an-id"], "invalid_asset_ids"),
            ("incinerate", ["aaaaaaaaaaaaaaaaaaaa"], "unknown_action"),
        ] {
            let (status, error) = send(&router, action_request("images", action, &ids)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{action}");
            assert_eq!(error["error"]["code"], code, "{action}");
        }

        // Deleting an already-deleted asset is a successful no-op.
        place_in_trash(&router, "images", &id).await;
        let (status, body) = send(&router, action_request("images", "delete", &[&id])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["status"], "deleted");
    }

    #[tokio::test]
    async fn srcset_describes_real_widths_and_falls_back_to_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        // A 500-pixel photo stored under four profiles.
        let small = write_test_image(&directory.path().join("small.png"), "500x375");
        let small_id = upload_image(&router, &small).await;
        let (status, body) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{small_id}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["src"],
            format!("/media/{small_id}/w640?g=1"),
            "the fallback src is the widest requested width that exists"
        );
        assert_eq!(
            body["srcset"],
            format!("/media/{small_id}/w320?g=1 320w, /media/{small_id}/w640?g=1 500w"),
            "descriptors are the widths that were actually produced"
        );

        // A width that was never generated simply does not appear.
        let (_, body) = send(
            &router,
            authorized(
                "GET",
                &format!("/api/v1/images/{small_id}/srcset?widths=960"),
            ),
        )
        .await;
        assert_eq!(body["src"], format!("/media/{small_id}/original"));
        assert!(
            body.get("srcset").is_none(),
            "no srcset key at all rather than an empty one"
        );

        // A document has no srcset route, and a document id is not an image.
        let document = upload_document(&router, b"%PDF-1.7\nno srcset").await;
        let (status, _) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{document}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, error) = send(
            &router,
            authorized(
                "GET",
                &format!("/api/v1/images/{small_id}/srcset?widths=lots"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_widths");

        // It is authenticated, like everything that describes stored bytes.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/images/{small_id}/srcset"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_recorded_rendition_whose_file_is_gone_is_an_internal_error() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("gone.png"), "800x600");
        let data_dir = directory.path().join("atlas");
        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![320, 640];
        let router = app(AppState::new(config).await.unwrap());

        let (status, body) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().unwrap().to_owned();
        for entry in std::fs::read_dir(data_dir.join("assets").join(&id)).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }

        let (status, error) = fetch_error(&router, &format!("/media/{id}/w320")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error["error"]["code"], "internal_error",
            "metadata and storage disagree, which is not a missing rendition"
        );
    }

    /// The configuration is visible, the srcset follows it, and an asset can
    /// gain a width it was uploaded without.
    ///
    /// A deployment that configures the wrong widths produces assets that can
    /// never gain the widths its site needs, and the only way to find out was
    /// to list every asset and read the renditions of each one.
    #[tokio::test]
    async fn the_configured_widths_are_reported_and_can_be_added() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "1400x900");
        let data_dir = directory.path().join("atlas");

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![400, 800];
        let router = app(AppState::new(config).await.unwrap());

        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();

        // The summary reports the widths this instance generates.
        let (status, summary) = send(&router, authorized("GET", "/api/v1/summary")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(summary["rendition_widths"], serde_json::json!([400, 800]));

        // The srcset describes those widths, not the ones the binary was
        // built with.
        let (status, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{id}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let candidates = srcset["srcset"].as_str().unwrap();
        assert!(candidates.contains(" 400w"), "{candidates}");
        assert!(candidates.contains(" 800w"), "{candidates}");
        assert!(!candidates.contains(" 960w"), "{candidates}");

        // The operator changes the configuration and restarts. The assets are
        // still the ones that were uploaded.
        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 400, 800];
        let router = app(AppState::new(config).await.unwrap());

        // The width the site needs is still missing: nothing this asset has
        // is narrow enough for it.
        let (status, _, _) = fetch(&router, &format!("/media/{id}/w200")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The rendition is generated in place, under the same asset id.
        let (status, refreshed) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{id}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["id"], id.as_str());
        assert_eq!(refreshed["widths"], serde_json::json!([200, 400, 800]));
        assert_eq!(refreshed["added"], serde_json::json!([200]));

        let (status, _, body) = fetch(&router, &format!("/media/{id}/w200")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());

        // The srcset now describes the new width as well.
        let (status, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{id}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(srcset["srcset"].as_str().unwrap().contains(" 200w"));

        // Running it again changes nothing.
        let (status, refreshed) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{id}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["added"], serde_json::json!([]));

        // The original is untouched, and the asset is still the same one.
        let (status, value) =
            send(&router, authorized("GET", &format!("/api/v1/images/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["id"], id.as_str());
    }

    /// A rendition that is already there keeps its bytes.
    ///
    /// A public URL that changes its bytes is a lie to every cache that holds
    /// it, so generating the missing widths must not touch the ones that
    /// exist.
    #[tokio::test]
    async fn adding_a_width_leaves_the_others_alone() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "1400x900");
        let data_dir = directory.path().join("atlas");

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 400];
        let router = app(AppState::new(config).await.unwrap());

        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();

        let (status, headers, before) = fetch(&router, &format!("/media/{id}/w200")).await;
        assert_eq!(status, StatusCode::OK);
        let etag = headers["etag"].to_str().unwrap().to_owned();

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 400, 800];
        let router = app(AppState::new(config).await.unwrap());
        let (status, _) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{id}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, headers, after) = fetch(&router, &format!("/media/{id}/w200")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            before, after,
            "the bytes behind a published URL do not change"
        );
        assert_eq!(headers["etag"].to_str().unwrap(), etag);
    }

    /// Several configured profiles can name the same actual width for a
    /// small image. Adding another such profile adds metadata that points at
    /// the already-stored bytes; it does not run ImageMagick over that path.
    #[tokio::test]
    async fn re_render_adds_aliases_for_profiles_that_share_an_actual_width() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "300x180");
        let data_dir = directory.path().join("atlas");

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 400];
        let router = app(AppState::new(config).await.unwrap());
        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();

        // The old w400 URL points to the 300-pixel file.
        let (status, headers_before, bytes_before) =
            fetch(&router, &format!("/media/{id}/w400")).await;
        assert_eq!(status, StatusCode::OK);
        let etag_before = headers_before["etag"].to_str().unwrap().to_owned();

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 400, 800];
        let router = app(AppState::new(config).await.unwrap());
        let (status, refreshed) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{id}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["added"], serde_json::json!([800]));

        let (status, headers_after, bytes_after) =
            fetch(&router, &format!("/media/{id}/w400")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes_before, bytes_after);
        assert_eq!(headers_after["etag"].to_str().unwrap(), etag_before);

        // The new configured profile is represented in srcset, but shares
        // the existing 300-pixel candidate rather than duplicating it.
        let (status, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{id}/srcset")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let candidates = srcset["srcset"].as_str().unwrap();
        assert!(candidates.contains(" 200w"), "{candidates}");
        assert!(
            candidates.contains(&format!("/media/{id}/w800?g=1 300w")),
            "{candidates}"
        );
        assert_eq!(candidates.matches("300w").count(), 1, "{candidates}");
    }

    /// Generating renditions is a change, so it needs the token.
    #[tokio::test]
    async fn generating_renditions_needs_the_token() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/images/{id}/renditions"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn re_encode_request(id: &str) -> Request<Body> {
        json_request(
            "POST",
            &format!("/api/v1/images/{id}/renditions"),
            &json!({"re_encode": true}),
        )
    }

    /// A re-encode writes a new generation beside the old one.
    ///
    /// The case it exists for is an encoding that turned out to be bad, so
    /// the test plants one: the first generation's PNG is swapped for a
    /// different picture before the re-encode. The URL that names the first
    /// generation keeps serving it, the new URLs serve the fresh encoding, and
    /// the asset is the same asset throughout.
    #[tokio::test]
    async fn a_re_encode_writes_a_new_generation_and_keeps_the_old_one() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "640x480");
        let data_dir = directory.path().join("atlas");
        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![320];
        let state = AppState::new(config).await.unwrap();
        let router = app(state.clone());

        let (status, uploaded) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = uploaded["id"].as_str().unwrap().to_owned();
        assert_eq!(uploaded["generation"], 1);
        assert_eq!(
            uploaded["renditions"][0]["url"],
            format!("/media/{id}/w320?g=1"),
            "the URLs the API hands out name their generation"
        );

        // The bad encoding: a blue picture where the red one should be.
        let asset_dir = data_dir.join("assets").join(&id);
        let planted = write_test_image_blue(&directory.path().join("blue.png"));
        std::fs::write(asset_dir.join("w320.png"), &planted).unwrap();
        state
            .store
            .replace_rendition_bytes(
                &AssetId::parse(&id).unwrap(),
                "w320.png",
                planted.len() as u64,
                &crate::media::sha256_bytes(&planted),
            )
            .await;

        let (status, refreshed) = send(&router, re_encode_request(&id)).await;
        assert_eq!(status, StatusCode::OK, "{refreshed}");
        assert_eq!(refreshed["id"], id.as_str());
        assert_eq!(refreshed["generation"], 2);
        assert_eq!(refreshed["added"], json!([]));
        assert_eq!(
            refreshed["renditions"][0]["url"],
            format!("/media/{id}/w320?g=2")
        );
        assert_eq!(refreshed["srcset"]["generation"], 2);
        assert_eq!(
            refreshed["srcset"]["srcset"],
            format!("/media/{id}/w320?g=2 320w"),
            "the answer carries the srcset a consumer stores"
        );

        // The URL that was handed out keeps its bytes, and stays immutable.
        let (status, headers, first) =
            fetch(&router, &format!("/media/{id}/w320?g=1&format=png")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first, planted, "the bytes behind a generation never change");
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );

        let (status, headers, second) =
            fetch(&router, &format!("/media/{id}/w320?g=2&format=png")).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(second, planted, "the new generation is a fresh encoding");
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );

        // The bare URL follows the current generation, and says so in its
        // cache policy.
        let (status, headers, latest) =
            fetch(&router, &format!("/media/{id}/w320?format=png")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(latest, second);
        assert_eq!(headers["cache-control"], "public, max-age=3600");

        // Negotiation works inside an old generation as it does in the new.
        let (status, headers, _) = fetch_with(
            &router,
            &format!("/media/{id}/w320?g=1"),
            Some("image/webp"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/webp");
        assert_eq!(headers["vary"], "Accept");

        // The asset, the srcset and the files say the same thing.
        let (_, asset) = send(&router, authorized("GET", &format!("/api/v1/images/{id}"))).await;
        assert_eq!(asset["id"], id.as_str());
        assert_eq!(asset["generation"], 2);
        let (_, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{id}/srcset")),
        )
        .await;
        assert_eq!(srcset["src"], format!("/media/{id}/w320?g=2"));
        assert_eq!(srcset["generation"], 2);
        assert!(asset_dir.join("w320.png").exists());
        assert!(asset_dir.join("w320-g2.png").exists());
        assert!(asset_dir.join("w320-g2.jxl").exists());

        // A generation the asset never had is not quietly the current one.
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w320?g=3")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "generation_not_found");
        for bad in ["0", "two", "-1"] {
            let (status, error) = fetch_error(&router, &format!("/media/{id}/w320?g={bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "g={bad}");
            assert_eq!(error["error"]["code"], "invalid_generation");
        }

        // A width added after the re-encode belongs to the new generation and
        // does not take an old file's name.
        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![200, 320];
        let router = app(AppState::new(config).await.unwrap());
        let (status, refreshed) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{id}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["generation"], 2);
        assert_eq!(refreshed["added"], json!([200]));
        assert!(asset_dir.join("w200-g2.png").exists());
        assert!(!asset_dir.join("w200.png").exists());
        let (status, _, _) = fetch(&router, &format!("/media/{id}/w200?g=2")).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A URL that names a generation keeps its bytes even when the asset
    /// gains a width.
    ///
    /// A profile the asset is missing falls back to a narrower rendition,
    /// and adding the profile later puts different bytes behind it. So the
    /// srcset never hands out such a URL, and serving one is not immutable.
    /// A profile that falls back because the photo is smaller than it is
    /// final already, and stays immutable.
    #[tokio::test]
    async fn a_generation_url_cannot_change_when_a_width_is_added() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());
        let large = upload_image(
            &router,
            &write_test_image(&directory.path().join("large.png"), "1400x900"),
        )
        .await;
        let small = upload_image(
            &router,
            &write_test_image(&directory.path().join("small.png"), "300x200"),
        )
        .await;

        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![320, 640, 1280];
        let router = app(AppState::new(config).await.unwrap());

        let (_, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{large}/srcset")),
        )
        .await;
        assert_eq!(srcset["src"], format!("/media/{large}/w640?g=1"));
        assert_eq!(
            srcset["srcset"],
            format!("/media/{large}/w320?g=1 320w, /media/{large}/w640?g=1 640w"),
            "a profile the asset does not have yet is not handed out"
        );
        let (status, headers, _) = fetch(&router, &format!("/media/{large}/w1280?g=1")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "public, max-age=3600");
        let (_, headers, _) = fetch(&router, &format!("/media/{large}/w640?g=1")).await;
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );
        let etag = headers["etag"].clone();

        // The small photo has nothing wider to give, so its widest profile is
        // final and can be handed out.
        let (_, srcset) = send(
            &router,
            authorized("GET", &format!("/api/v1/images/{small}/srcset")),
        )
        .await;
        assert_eq!(srcset["srcset"], format!("/media/{small}/w1280?g=1 300w"));
        let (_, headers, _) = fetch(&router, &format!("/media/{small}/w1280?g=1")).await;
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );

        let (status, refreshed) = send(
            &router,
            authorized("POST", &format!("/api/v1/images/{large}/renditions")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refreshed["added"], json!([1280]));
        assert_eq!(
            refreshed["srcset"]["src"],
            format!("/media/{large}/w1280?g=1")
        );
        let (_, headers, _) = fetch(&router, &format!("/media/{large}/w1280?g=1")).await;
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );
        let (_, headers, _) = fetch(&router, &format!("/media/{large}/w640?g=1")).await;
        assert_eq!(headers["etag"], etag, "the URL handed out kept its bytes");
    }

    fn write_test_image_blue(path: &std::path::Path) -> Vec<u8> {
        let generated = Command::new("magick")
            .args(["-size", "320x240", "xc:blue"])
            .arg(format!("PNG:{}", path.display()))
            .output()
            .unwrap();
        assert!(generated.status.success());
        std::fs::read(path).unwrap()
    }

    /// A re-encode that cannot start leaves the asset where it was.
    #[tokio::test]
    async fn a_re_encode_is_refused_rather_than_guessed_at() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        let (status, uploaded) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = uploaded["id"].as_str().unwrap().to_owned();

        // A misspelt option is an error, not the other mode running instead.
        let (status, error) = send(
            &router,
            json_request(
                "POST",
                &format!("/api/v1/images/{id}/renditions"),
                &json!({"reencode": true}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_request_body");

        // An explicit `false` is the repair, as an empty or `null` body is.
        for body in [json!({"re_encode": false}), json!({}), json!(null)] {
            let (status, refreshed) = send(
                &router,
                json_request("POST", &format!("/api/v1/images/{id}/renditions"), &body),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(refreshed["generation"], 1, "{body}");
        }

        // A trashed image has no bytes to re-encode.
        place_in_trash(&router, "images", &id).await;
        let (status, error) = send(&router, re_encode_request(&id)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["error"]["code"], "asset_retired");

        // A document has no renditions, so there is no such image.
        let document = upload_document(&router, b"%PDF-1.7\nno renditions").await;
        let (status, _) = send(&router, re_encode_request(&document)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The re-encode meets the same fidelity floors as the upload.
    ///
    /// It runs the same encoder settings, but it reaches them by its own
    /// path, and a path that passed different widths or a different source
    /// would pass every other test here while making the pictures worse.
    #[tokio::test]
    async fn a_re_encode_stays_close_to_the_source() {
        use crate::media::tests::{
            FIDELITY_WIDTH, REFERENCE_IMAGE, assert_renditions_are_faithful,
        };

        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let mut config = test_config(data_dir.clone());
        config.variant_widths = vec![FIDELITY_WIDTH];
        let state = AppState::new(config).await.unwrap();
        let router = app(state.clone());

        let (status, uploaded) = send(
            &router,
            upload_request("image/jpeg", std::fs::read(REFERENCE_IMAGE).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = AssetId::parse(uploaded["id"].as_str().unwrap()).unwrap();
        let (status, _) = send(&router, re_encode_request(id.as_str())).await;
        assert_eq!(status, StatusCode::OK);

        let stored = state.store.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(stored.asset.generation, 2);
        assert_eq!(stored.renditions.len(), 4, "every format is re-encoded");
        assert!(
            stored
                .renditions
                .iter()
                .all(|rendition| rendition.file_name.contains("-g2.")),
            "the current rows point at the new generation's files"
        );
        let scratch = directory.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        assert_renditions_are_faithful(
            &state.image_processor,
            std::path::Path::new(REFERENCE_IMAGE),
            &stored.renditions,
            &state.asset_dir(&id),
            &scratch,
        )
        .await;
    }

    /// Every live image, re-encoded a page at a time, with the progress in
    /// each answer.
    #[tokio::test]
    async fn re_render_walks_every_live_image_a_page_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap();
        let router = app(state.clone());

        let mut live = Vec::new();
        for size in ["400x300", "500x300"] {
            let image = write_test_image(&directory.path().join(format!("{size}.png")), size);
            live.push(upload_image(&router, &image).await);
        }
        let trashed = upload_image(
            &router,
            &write_test_image(&directory.path().join("trashed.png"), "600x300"),
        )
        .await;
        place_in_trash(&router, "images", &trashed).await;
        upload_document(&router, b"%PDF-1.7\nnot an image").await;
        live.sort();

        let (status, page) = send(
            &router,
            json_request("POST", "/api/v1/images/re-render", &json!({"limit": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["count"], 1);
        assert_eq!(
            page["re_encoded"],
            json!([{"id": live[0], "generation": 2}])
        );
        assert_eq!(page["failures"], json!([]));
        assert_eq!(page["remaining"], 1);
        assert_eq!(page["next"], live[0].as_str());

        let (status, page) = send(
            &router,
            json_request(
                "POST",
                "/api/v1/images/re-render",
                &json!({"limit": 1, "after": live[0]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["count"], 1);
        assert_eq!(page["re_encoded"][0]["id"], live[1].as_str());
        assert_eq!(page["remaining"], 0);
        assert!(page["next"].is_null(), "the walk is done");

        for id in &live {
            let (_, asset) = send(&router, authorized("GET", &asset_uri("images", id))).await;
            assert_eq!(asset["generation"], 2, "{id}");
        }
        let (_, asset) = send(&router, authorized("GET", &asset_uri("images", &trashed))).await;
        assert_eq!(asset["generation"], 1, "the trash is not re-encoded");

        // An empty body starts from the beginning with the default page.
        let (status, page) = send(&router, authorized("POST", "/api/v1/images/re-render")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["count"], 2);

        let (status, error) = send(
            &router,
            json_request(
                "POST",
                "/api/v1/images/re-render",
                &json!({"after": "../x"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_cursor");

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/images/re-render")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A caller can ask for a rendition format by name.
    ///
    /// The `Accept` header is for a browser that cannot choose for itself. A
    /// crawler fetching an image for a marketplace needs a format the
    /// marketplace accepts, and the marketplace's list does not always
    /// include the newest one Atlas has.
    #[tokio::test]
    async fn a_caller_can_name_the_format_it_wants() {
        let directory = tempfile::tempdir().unwrap();
        let image = write_test_image(&directory.path().join("source.png"), "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();

        // The format in the URL wins over the header, whatever the header
        // says.
        let (status, headers, body) = fetch_with(
            &router,
            &format!("/media/{id}/w320?format=webp"),
            Some("image/avif,image/webp,image/*;q=0.8"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/webp");
        assert_eq!(&body[..4], b"RIFF", "the WebP rendition is served");
        assert!(
            !headers.contains_key("vary"),
            "a URL that names its format does not vary by Accept"
        );

        // Without the parameter the negotiation is exactly as it was.
        let (status, headers, _) = fetch_with(
            &router,
            &format!("/media/{id}/w320"),
            Some("image/avif,image/webp,image/*;q=0.8"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/avif");
        assert_eq!(headers["vary"], "Accept");

        // A format the asset does have is served whatever the header says.
        let (status, headers, _) = fetch_with(
            &router,
            &format!("/media/{id}/w320?format=png"),
            Some("image/avif"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/png");

        // An unknown format is refused, with the usual error body.
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w320?format=tiff")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_format");

        // A format the asset does not have at that width is a 404: this image
        // is a PNG, so its fallback is a PNG and it has no JPEG rendition.
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w320?format=jpeg")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "rendition_format_not_found");
    }

    #[tokio::test]
    async fn rendition_urls_also_fall_back_to_the_placeholder() {
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

        let (status, value) = send(&router, upload_request("image/png", image)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = value["id"].as_str().unwrap().to_owned();
        place_in_trash(&router, "images", &id).await;

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}/w320"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.headers()["cache-control"], "public, max-age=60");

        // Even though a retired asset is represented by a placeholder, an
        // unrecognized format name is still a malformed request.
        let (status, error) = fetch_error(&router, &format!("/media/{id}/w320?format=tiff")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["error"]["code"], "invalid_format");
    }

    #[tokio::test]
    async fn trash_listing_and_pinning_are_authenticated() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let router = app(state);
        let id = upload_document(&router, b"%PDF-1.7\nlisted").await;

        let unauthorized = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/trash")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["assets"].as_array().unwrap().len(), 0);

        place_in_trash(&router, "documents", &id).await;
        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["assets"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], id.as_str());
        assert_eq!(entries[0]["state"], "deleted");
        assert_eq!(entries[0]["content_type"], "application/pdf");

        let (status, body) = send(&router, pin_request("documents", &id, true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pinned"], true);

        let unknown = "aaaaaaaaaaaaaaaaaaaa";
        let (status, _) = send(&router, pin_request("documents", unknown, true)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "an unknown id has no row to pin"
        );
    }

    #[tokio::test]
    async fn the_trash_listing_is_bounded_and_pageable() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.trash_retention_days = 30;
        let state = AppState::new(config.clone()).await.unwrap();
        let router = app(state.clone());

        for index in 0..5 {
            let body = format!("%PDF-1.7\ntrash {index}").into_bytes();
            let (status, value) = send(&router, upload_request("application/pdf", body)).await;
            assert_eq!(status, StatusCode::CREATED);
            let id = value["id"].as_str().unwrap().to_owned();
            place_in_trash(&router, "documents", &id).await;
        }
        drop(router);

        // Restore one so the trash is not uniform, then restart so that one is
        // the newest deletion on every page.
        drop(state);
        let router = app(AppState::new(config.clone()).await.unwrap());
        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 5);
        assert_eq!(body["limit"], 200);
        assert_eq!(body["offset"], 0);
        assert_eq!(body["assets"].as_array().unwrap().len(), 5);

        let (_, first) = send(&router, authorized("GET", "/api/v1/trash?limit=2&offset=0")).await;
        assert_eq!(first["assets"].as_array().unwrap().len(), 2);
        assert_eq!(first["total"], 5, "the count is of the whole trash");

        let (_, second) = send(&router, authorized("GET", "/api/v1/trash?limit=2&offset=2")).await;
        let (_, third) = send(&router, authorized("GET", "/api/v1/trash?limit=2&offset=4")).await;
        let ids = |page: &serde_json::Value| -> Vec<String> {
            page["assets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let mut paged = ids(&first);
        paged.extend(ids(&second));
        paged.extend(ids(&third));
        assert_eq!(paged.len(), 5);
        assert_eq!(
            paged.iter().collect::<std::collections::HashSet<_>>().len(),
            5,
            "pages do not overlap or skip"
        );

        // A page past the end is empty rather than an error.
        let (status, beyond) = send(
            &router,
            authorized("GET", "/api/v1/trash?limit=2&offset=99"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(beyond["assets"].as_array().unwrap().is_empty());
        assert_eq!(beyond["total"], 5);
    }

    #[tokio::test]
    async fn a_trash_page_is_capped_so_a_caller_cannot_ask_for_everything() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());
        let (status, body) = send(&router, authorized("GET", "/api/v1/trash?limit=100000")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["limit"], 500);

        let (status, body) = send(&router, authorized("GET", "/api/v1/trash?offset=-5")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["offset"], 0);
    }

    #[tokio::test]
    async fn unknown_ids_stay_not_found_after_a_delete() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().to_owned()))
            .await
            .unwrap();
        let router = app(state);
        let unknown = "aaaaaaaaaaaaaaaaaaaa";

        for (method, uri) in [
            ("DELETE", asset_uri("documents", unknown)),
            (
                "POST",
                format!("{}/restore", asset_uri("documents", unknown)),
            ),
            ("GET", asset_uri("documents", unknown)),
        ] {
            let (status, _) = send(&router, authorized(method, &uri)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }

        for uri in [
            format!("/media/{unknown}/original"),
            format!("/media/{unknown}/w320"),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn purge_keeps_a_tombstone_and_skips_pinned_assets() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let mut config = test_config(data_dir.clone());
        config.trash_retention_days = 30;

        let state = AppState::new(config.clone()).await.unwrap();
        let expired = upload_document(&app(state.clone()), b"%PDF-1.7\nexpired").await;
        let kept = upload_document(&app(state.clone()), b"%PDF-1.7\tkept").await;
        let router = app(state.clone());
        let (status, _) = send(
            &router,
            json_request(
                "PUT",
                &format!("/api/v1/documents/{expired}/data"),
                &serde_json::json!({"data": "the consumer's note outlives its bytes"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        place_in_trash(&router, "documents", &expired).await;
        place_in_trash(&router, "documents", &kept).await;

        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["assets"].as_array().unwrap().len(), 2);

        let (status, _) = send(&router, pin_request("documents", &kept, true)).await;
        assert_eq!(status, StatusCode::OK);

        let long_ago = crate::unix_now().unwrap() - 40 * 24 * 60 * 60;
        for id in [&expired, &kept] {
            state
                .store
                .backdate_deletion(&AssetId::parse(id).unwrap(), long_ago)
                .await;
        }
        drop(state);
        drop(router);

        // A restart is when the retention window is enforced.
        let state = AppState::new(config).await.unwrap();
        let router = app(state);
        assert!(!data_dir.join("trash").join(&expired).exists());
        assert!(data_dir.join("trash").join(&kept).exists());

        let (status, body) = send(
            &router,
            authorized("GET", &asset_uri("documents", &expired)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "purged");
        assert_eq!(
            body["data"], "the consumer's note outlives its bytes",
            "tombstoning the media does not erase what the consumer knows"
        );
        assert!(body["purged_at"].is_i64());
        assert_eq!(
            body["renditions"].as_array().unwrap().len(),
            0,
            "a tombstone does not advertise bytes that are gone"
        );

        let (status, body) = send(&router, authorized("GET", &asset_uri("documents", &kept))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "deleted");
        assert_eq!(body["pinned"], true);

        let (status, body) = send(&router, authorized("GET", "/api/v1/trash")).await;
        assert_eq!(status, StatusCode::OK);
        let mut states: Vec<_> = body["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["state"].as_str().unwrap().to_owned())
            .collect();
        states.sort();
        assert_eq!(states, vec!["deleted", "purged"]);

        let (status, _) = send(
            &router,
            authorized(
                "POST",
                &format!("{}/restore", asset_uri("documents", &expired)),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "purged bytes cannot come back"
        );

        let (status, _) = send(
            &router,
            authorized(
                "POST",
                &format!("{}/restore", asset_uri("documents", &kept)),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn recovery_returns_an_interrupted_delete_to_the_trash() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let config = test_config(data_dir.clone());
        let state = AppState::new(config.clone()).await.unwrap();
        let id = upload_document(&app(state.clone()), b"%PDF-1.7\ninterrupted").await;
        drop(state);

        // Crash between recording the delete and moving the files.
        let state = AppState::new(config.clone()).await.unwrap();
        state
            .store
            .mark_deleted(&AssetId::parse(&id).unwrap(), crate::unix_now().unwrap())
            .await
            .unwrap();
        drop(state);

        let _state = AppState::new(config).await.unwrap();
        assert!(!data_dir.join("assets").join(&id).exists());
        assert!(data_dir.join("trash").join(&id).exists());
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
    async fn an_uploaded_name_is_reported_and_used_for_downloads() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());

        let (status, body) = send(
            &router,
            upload_named("application/pdf", b"%PDF-1.7\nnamed", "menu-2026.pdf"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().unwrap().to_owned();
        assert_eq!(body["original"]["filename"], "menu-2026.pdf");

        // The name is metadata, never a path: the file on disk is still named
        // after the asset ID and the detected format.
        assert!(
            data_dir
                .join("assets")
                .join(&id)
                .join("original.pdf")
                .exists()
        );
        assert!(
            !data_dir
                .join("assets")
                .join(&id)
                .join("menu-2026.pdf")
                .exists()
        );

        let (status, headers, body) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"%PDF-1.7\nnamed");
        assert_eq!(
            headers["content-disposition"],
            "inline; filename=\"menu-2026.pdf\"; filename*=UTF-8''menu-2026.pdf",
            "an ASCII name is sent twice: readable, and exact"
        );
    }

    #[tokio::test]
    async fn a_percent_encoded_name_survives_the_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let encoded = "Jahresbericht%20%28%C3%9C%29.pdf";
        let (status, body) = send(
            &router,
            upload_named("application/pdf", b"%PDF-1.7\nunicode", encoded),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            body["original"]["filename"], "Jahresbericht (Ü).pdf",
            "the API reports the decoded name"
        );
        let id = body["id"].as_str().unwrap().to_owned();

        let (_, headers, _) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(
            headers["content-disposition"],
            "inline; filename=\"Jahresbericht (_).pdf\"; \
             filename*=UTF-8''Jahresbericht%20%28%C3%9C%29.pdf"
        );
    }

    #[tokio::test]
    async fn a_hostile_name_is_refused_and_never_reaches_a_path() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());

        for (index, (name, why)) in [
            ("evil%0D%0AX-Injected:%20yes", "a CRLF injection attempt"),
            ("line%0Abreak.pdf", "a newline"),
            ("nul%00byte.pdf", "a NUL byte"),
            ("%2F%2F", "nothing but a path"),
            ("%FF%FE", "bytes that are not text"),
        ]
        .into_iter()
        .enumerate()
        {
            let (status, error) = send(
                &router,
                upload_named(
                    "application/pdf",
                    format!("%PDF-1.7\nhostile {index}").as_bytes(),
                    name,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {why}");
            assert_eq!(error["error"]["code"], "invalid_filename", "{name}");
        }

        // A path is stripped rather than refused: the name is a label, not a
        // location, and what survives is the last segment.
        for (index, (sent, expected)) in [
            ("../../etc/passwd", "passwd"),
            ("..%2F..%2Fsecret%2Fphoto.pdf", "photo.pdf"),
            ("C%3A%5CUsers%5Cme%5Cphoto.pdf", "photo.pdf"),
        ]
        .into_iter()
        .enumerate()
        {
            let (status, body) = send(
                &router,
                upload_named(
                    "application/pdf",
                    format!("%PDF-1.7\ntraversal {index}").as_bytes(),
                    sent,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{sent}");
            assert_eq!(body["original"]["filename"], expected, "{sent}");
            let id = body["id"].as_str().unwrap().to_owned();
            assert!(
                data_dir
                    .join("assets")
                    .join(&id)
                    .join("original.pdf")
                    .exists()
            );
        }
    }

    #[tokio::test]
    async fn a_name_cannot_change_the_stored_format() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("atlas");
        let router = app(AppState::new(test_config(data_dir.clone())).await.unwrap());

        let (status, body) = send(
            &router,
            upload_named("application/pdf", b"%PDF-1.7\nmismatch", "payload.png"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["original"]["content_type"], "application/pdf");
        let id = body["id"].as_str().unwrap().to_owned();

        let stored = std::fs::read_dir(data_dir.join("assets").join(&id))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            stored,
            vec!["original.pdf".to_owned()],
            "the name picks nothing"
        );

        let (_, headers, _) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(headers["content-type"], "application/pdf");
    }

    #[tokio::test]
    async fn an_upload_without_a_name_keeps_the_generic_download_name() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, body) = send(
            &router,
            upload_request("application/pdf", b"%PDF-1.7\nplain".to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["original"]["filename"], serde_json::Value::Null);
        let id = body["id"].as_str().unwrap().to_owned();

        let (_, headers, _) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(
            headers["content-disposition"], "inline; filename=\"asset.pdf\"",
            "an upload without a name keeps Atlas's generic download name"
        );
    }

    #[tokio::test]
    async fn images_are_served_inline_with_their_name() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("named.png");
        let image = write_test_image(&source, "640x480");
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        // A raw UTF-8 name needs no encoding, which is what a browser's
        // `File.name` would send.
        let (status, body) = send(
            &router,
            upload_named("image/png", &image, "family \u{1F600}.png"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["original"]["filename"], "family \u{1F600}.png");
        let id = body["id"].as_str().unwrap().to_owned();

        let (status, headers, _) = fetch(&router, &format!("/media/{id}/original")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers["content-disposition"],
            "inline; filename=\"family _.png\"; filename*=UTF-8''family%20%F0%9F%98%80.png"
        );

        // Renditions are Atlas's own output, so they keep the generic name.
        let (_, headers, _) = fetch(&router, &format!("/media/{id}/w320")).await;
        assert_eq!(
            headers["content-disposition"],
            "inline; filename=\"asset.png\""
        );
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
                    .uri("/api/v1/images")
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
                        .uri(asset_uri("images", id))
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

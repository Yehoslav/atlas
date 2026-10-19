use std::{
    collections::BTreeMap,
    io::SeekFrom,
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Body,
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
    created_at: i64,
    kind: Kind,
    state: AssetState,
    deleted_at: Option<i64>,
    purged_at: Option<i64>,
    pinned: bool,
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
                    url: format!("/media/{}/w{max_width}", asset.id),
                    formats,
                }
            })
            .collect();

        Self {
            id: asset.id.to_string(),
            deduplicated: None,
            created_at: asset.created_at,
            kind: asset.kind.unwrap_or_else(|| asset.source_format.kind()),
            state: asset.state(),
            deleted_at: asset.deleted_at,
            purged_at: asset.purged_at,
            pinned: asset.pinned,
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

impl From<&StoredAsset> for TrashEntryResponse {
    fn from(value: &StoredAsset) -> Self {
        let asset = &value.asset;
        Self {
            id: asset.id.to_string(),
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
                Kind::Image => {
                    "The images collection accepts image/jpeg, image/png, image/webp, \
                     image/avif, image/jxl and image/heic. GET /api/v1/formats lists them."
                }
                Kind::Document => {
                    "This file type is not accepted. GET /api/v1/formats lists what is."
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
    Ok(Json(AssetResponse::from(&asset)))
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

/// The media types of a set of formats, in the order they were given.
fn mime_list(formats: &[SourceFormat]) -> Vec<&'static str> {
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
    }))
}

#[derive(Default, Serialize)]
pub(super) struct CollectionSummary {
    count: i64,
    bytes: i64,
}

/// What each collection accepts.
///
/// Derived from the same enum the upload path validates against, so this cannot
/// describe a list Atlas will then reject: a consumer building an upload form
/// reads it instead of hardcoding types that drift on the next release.
pub(super) async fn list_formats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<FormatsResponse>, ApiError> {
    authorize(&state, &headers)?;
    let for_kind = |kind: Kind| -> AcceptedFormats {
        let formats = SourceFormat::ALL
            .into_iter()
            .filter(|format| kind.accepts(*format))
            .collect::<Vec<_>>();
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
            formats: mime_list(&formats),
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
    formats: Vec<&'static str>,
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
    let widths = params.widths()?;

    // One entry per width: each stored rendition has one row per *format*, and
    // a srcset descriptor describes a width, not a representation.
    let mut chosen: Vec<&RenditionRecord> = Vec::new();
    for width in widths {
        let Some(rendition) = asset
            .renditions
            .iter()
            .find(|rendition| rendition.max_width == width)
        else {
            continue;
        };
        chosen.push(rendition);
    }
    chosen.sort_by_key(|rendition| rendition.max_width);

    // The fallback `src` is the widest width that was asked for and exists, or
    // the original when none of the requested widths were generated.
    let src = match chosen.last() {
        Some(rendition) => format!("/media/{id}/w{}", rendition.max_width),
        None => format!("/media/{id}/original"),
    };

    let srcset = (!chosen.is_empty()).then(|| {
        chosen
            .iter()
            .map(|rendition| format!("/media/{id}/w{} {}w", rendition.max_width, rendition.width))
            .collect::<Vec<_>>()
            .join(", ")
    });

    Ok(Json(SrcsetResponse { src, srcset }))
}

#[derive(Deserialize)]
pub(super) struct SrcsetParams {
    widths: Option<String>,
}

impl SrcsetParams {
    fn widths(&self) -> Result<Vec<u32>, ApiError> {
        let raw = self.widths.as_deref().unwrap_or("320,640,960,1280");
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
    let asset = load_asset(&state, &id, None).await?;
    if let Some(response) = serve_placeholder_if_retired(&headers, &asset) {
        return Ok(response);
    }

    let (selected, path) = resolve_rendition(&state, &asset, &headers, max_width)?;
    serve_file(
        &headers,
        &path,
        selected.bytes,
        &selected.sha256,
        None,
        selected.format.mime_type(),
        "inline",
        true,
    )
    .await
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
fn resolve_rendition<'a>(
    state: &AppState,
    asset: &'a StoredAsset,
    headers: &HeaderMap,
    max_width: u32,
) -> Result<(&'a RenditionRecord, std::path::PathBuf), ApiError> {
    let group: Vec<&RenditionRecord> = {
        let exact: Vec<_> = asset
            .renditions
            .iter()
            .filter(|rendition| rendition.max_width == max_width)
            .collect();
        if !exact.is_empty() {
            exact
        } else {
            let widest = asset
                .renditions
                .iter()
                .filter(|rendition| rendition.width <= max_width)
                .map(|rendition| rendition.width)
                .max();
            let Some(widest) = widest else {
                let mut available: Vec<u32> = asset
                    .renditions
                    .iter()
                    .map(|rendition| rendition.max_width)
                    .collect();
                available.sort_unstable();
                available.dedup();
                return Err(ApiError::rendition_not_found(max_width, &available));
            };
            // Every stored representation of that one width, so format
            // negotiation still chooses between them.
            asset
                .renditions
                .iter()
                .filter(|rendition| rendition.width == widest)
                .collect()
        }
    };

    let selected = select_format(headers, &group).ok_or_else(ApiError::not_acceptable)?;
    let path = state.asset_dir(&asset.asset.id).join(&selected.file_name);
    Ok((selected, path))
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
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
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

    /// A consumer building an upload form should be able to ask rather than
    /// hardcode a list that drifts on the next release.
    #[tokio::test]
    async fn the_accepted_formats_are_readable_and_true() {
        let directory = tempfile::tempdir().unwrap();
        let router = app(AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap());

        let (status, body) = send(&router, authorized("GET", "/api/v1/formats")).await;
        assert_eq!(status, StatusCode::OK);
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
        assert_eq!(body["images"]["groups"], json!(["image/*"]));
        assert_eq!(body["images"]["generates_renditions"], json!(true));

        // Documents take everything, which is the point of the collection.
        let documents = body["documents"]["formats"].as_array().unwrap();
        assert_eq!(documents.len(), SourceFormat::ALL.len());
        assert_eq!(
            body["documents"]["groups"],
            json!(["application/*", "image/*"])
        );
        assert_eq!(body["documents"]["generates_renditions"], json!(false));

        // Nothing listed may be refused by the endpoint that validates it, so
        // each advertised type is offered bytes that really are that type.
        for content_type in images {
            let content_type = content_type.as_str().unwrap();
            let format = SourceFormat::parse_content_type(content_type).unwrap();
            let bytes = write_test_image_as(
                &directory
                    .path()
                    .join(format!("probe.{}", format.extension())),
                "20x20",
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
            format!("/media/{small_id}/w640"),
            "the fallback src is the widest requested width that exists"
        );
        assert_eq!(
            body["srcset"],
            format!("/media/{small_id}/w320 320w, /media/{small_id}/w640 500w"),
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

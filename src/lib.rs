mod api;
mod backup;
pub mod config;
mod disk;
mod error;
mod filename;
mod ident;
mod media;
mod model;
mod store;

use std::{
    collections::HashMap,
    error::Error,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::Response,
};
use ident::AssetId;
use media::ImageProcessor;
use model::AssetState;
use store::Store;
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

pub use config::Config;
pub use ident::AssetId as PublicAssetId;

/// How long the opportunistic purge on upload may be skipped. Purge also runs
/// at startup, so this only bounds how often trash is swept on a busy host.
const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct AppState {
    pub(crate) config: Arc<Config>,
    pub(crate) store: Store,
    pub(crate) image_processor: ImageProcessor,
    pub(crate) upload_slots: Arc<Semaphore>,
    pub(crate) conversion_slots: Arc<Semaphore>,
    asset_conversion_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    pub(crate) last_purge: Arc<Mutex<Option<Instant>>>,
    /// Serializes backup creation against trash purging and against another
    /// backup. A purge must not remove bytes between the database snapshot and
    /// the file copy, and two concurrent backups would double the disk cost
    /// this operation already checks for.
    pub(crate) backup_lock: Arc<Mutex<()>>,
    /// Normal requests take a read guard; applying a restore takes the write
    /// guard, so the file swap happens with no request reading or writing the
    /// store it is replacing.
    pub(crate) restore_gate: Arc<tokio::sync::RwLock<()>>,
    /// A backup holds the write side while it snapshots and copies; delete and
    /// restore-from-trash hold the read side across their rename and row
    /// update. Without this, an asset could move between `assets/` and
    /// `trash/` after the snapshot and land only on the wrong side of the
    /// bundle.
    pub(crate) lifecycle_lock: Arc<tokio::sync::RwLock<()>>,
    /// The OpenAPI document, built once in `app()` from the same handlers the
    /// router serves. Empty until then, which is why the handler expects it.
    pub(crate) openapi: Arc<OnceLock<utoipa::openapi::OpenApi>>,
}

impl AppState {
    pub async fn new(mut config: Config) -> Result<Self, Box<dyn Error + Send + Sync>> {
        create_private_dir(&config.data_dir, false).await?;
        config.data_dir = tokio::fs::canonicalize(&config.data_dir).await?;
        create_private_dir(&config.data_dir.join("staging"), true).await?;
        create_private_dir(&config.data_dir.join("assets"), true).await?;
        create_private_dir(&config.data_dir.join("trash"), true).await?;
        create_private_dir(&config.data_dir.join("backups"), true).await?;

        // A restore that crashed mid-swap leaves a journal. Finish or undo the
        // swap before opening a database, so a missing file cannot become a
        // freshly created empty one and a later recovery cannot delete the
        // asset directories that belong to it.
        backup::recover_interrupted_restore(&config.data_dir).await?;

        let store = Store::connect(&config.data_dir.join("atlas.sqlite3")).await?;
        recover_incomplete_storage(&config.data_dir, &store).await?;
        // `-profile` takes a file, so the embedded profile is written out.
        let srgb_profile = config.data_dir.join("sRGB.icc");
        tokio::fs::write(&srgb_profile, media::SRGB_PROFILE).await?;
        let image_processor = ImageProcessor::new(
            config.magick_bin.clone(),
            srgb_profile,
            config.max_image_pixels,
            config.max_image_dimension,
            config.magick_timeout,
        );
        image_processor.verify_formats().await?;
        image_processor.verify_colour_management().await?;

        let conversion_concurrency = config.conversion_concurrency;
        let state = Self {
            config: Arc::new(config),
            store,
            image_processor,
            upload_slots: Arc::new(Semaphore::new(2)),
            conversion_slots: Arc::new(Semaphore::new(conversion_concurrency)),
            asset_conversion_locks: Arc::new(Mutex::new(HashMap::new())),
            last_purge: Arc::new(Mutex::new(None)),
            backup_lock: Arc::new(Mutex::new(())),
            restore_gate: Arc::new(tokio::sync::RwLock::new(())),
            lifecycle_lock: Arc::new(tokio::sync::RwLock::new(())),
            openapi: Arc::new(OnceLock::new()),
        };

        match state.purge_expired_trash().await {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "purged expired trashed assets at startup"),
            Err(error) => tracing::error!(%error, "startup trash purge failed"),
        }

        // A restart onto a full disk must still start, serve reads, and let the
        // operator purge: warning is the whole response here. Writes refuse
        // themselves when they see the floor, which is the one place refusing
        // is safe.
        match disk::space(&state.config.data_dir) {
            Ok(space) if space.free_bytes < state.config.min_free_bytes => {
                tracing::warn!(
                    free_bytes = space.free_bytes,
                    min_free_bytes = state.config.min_free_bytes,
                    "data directory is below the configured free-space floor; uploads and re-renders will be refused"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "cannot read free space for the data directory");
            }
        }

        // Jobs the previous process left queued or running continue here,
        // before the service answers requests: the connection that started
        // them is long gone, which is the point of a job.
        api::resume_re_render_jobs(state.clone()).await;

        Ok(state)
    }

    pub(crate) fn asset_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("assets").join(id.as_str())
    }

    pub(crate) fn staging_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("staging").join(id.as_str())
    }

    pub(crate) fn trash_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("trash").join(id.as_str())
    }

    pub(crate) fn backups_dir(&self) -> PathBuf {
        self.config.data_dir.join("backups")
    }

    /// Where an unfinished bundle is written before it is renamed into
    /// `backups/`: an incomplete backup is just another staging artifact.
    pub(crate) fn backup_staging_dir(&self) -> PathBuf {
        self.config.data_dir.join("staging").join("backups")
    }

    pub(crate) fn backup_path(&self, id: &str) -> PathBuf {
        self.backups_dir().join(format!("{id}.tar"))
    }

    /// A restore upload and its extracted tree, under staging so startup
    /// discards an upload that was never applied.
    pub(crate) fn restore_staging_dir(&self, upload_id: &str) -> PathBuf {
        self.config
            .data_dir
            .join("staging")
            .join("restores")
            .join(upload_id)
    }

    /// Serialize conversions of the same asset while allowing distinct assets
    /// to use the configured global conversion capacity concurrently.
    pub(crate) async fn lock_asset_conversion(&self, id: &AssetId) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.asset_conversion_locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(id.as_str()).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(id.to_string(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    /// Discard trashed assets that are past the retention window and that no
    /// consumer has pinned. Runs at startup and, throttled, after an upload.
    pub(crate) async fn purge_expired_trash(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        if !self.claim_purge_slot().await {
            return Ok(0);
        }

        // A backup copies `trash/` after snapshotting the database, so a purge
        // running in between could remove a directory the snapshot names. The
        // same lock makes backup creation and purging take turns. The restore
        // gate keeps a scheduled purge out of a restore's file swap.
        let _restore_guard = self.restore_gate.read().await;
        let _backup_guard = self.backup_lock.lock().await;

        let now = unix_now()?;
        let retention = i64::from(self.config.trash_retention_days).saturating_mul(24 * 60 * 60);
        let candidates = self.store.purge_candidates(now - retention).await?;
        let mut purged = 0;
        for candidate in candidates {
            let Ok(id) = AssetId::parse(&candidate) else {
                tracing::error!("stored asset id is not a valid identifier: {candidate}");
                continue;
            };

            // Tombstone first: a crash here leaves bytes that startup recovery
            // removes, while the reverse order would publish an asset whose
            // bytes are already gone.
            if !self.store.mark_purged(&id, now).await? {
                continue;
            }
            match tokio::fs::remove_dir_all(self.trash_dir(&id)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Box::new(error)),
            }
            purged += 1;
        }

        if purged > 0 {
            tracing::info!(purged, "purged expired trashed assets");
        }
        Ok(purged)
    }

    /// Claim the right to sweep, so concurrent uploads purge at most once per
    /// interval. The timestamp is taken before the work starts.
    async fn claim_purge_slot(&self) -> bool {
        let mut last = self.last_purge.lock().await;
        if last.is_some_and(|previous| previous.elapsed() < PURGE_INTERVAL) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

pub(crate) fn unix_now() -> Result<i64, Box<dyn Error + Send + Sync>> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(elapsed.as_secs() as i64)
}

async fn create_private_dir(path: &Path, allow_tighten_existing: bool) -> std::io::Result<()> {
    let existed = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Atlas storage paths must be real directories, not files or symlinks",
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };

    tokio::fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = tokio::fs::symlink_metadata(path).await?;
        let permissions = metadata.permissions().mode();
        if existed && permissions & 0o077 != 0 && !allow_tighten_existing {
            let mut entries = tokio::fs::read_dir(path).await?;
            if entries.next_entry().await?.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "Atlas storage directory {} has unsafe permissions {:o}; make it private before starting Atlas",
                        path.display(),
                        permissions & 0o777
                    ),
                ));
            }
        }
        if !existed || permissions & 0o077 != 0 {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
        }
    }
    Ok(())
}

async fn remove_path(path: &Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Bring the stored files back in line with the index after a crash.
///
/// Staging is always discarded, and a directory with no row is un-published
/// work. Anything else is moved back to where its row says it belongs, so an
/// interrupted delete or restore converges. The trash itself is left alone: a
/// trashed asset is a deliberate state, not a crash artifact, and only the
/// retention sweep discards it.
pub(crate) async fn recover_incomplete_storage(
    data_dir: &Path,
    store: &Store,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let staging_dir = data_dir.join("staging");
    let mut entries = tokio::fs::read_dir(&staging_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        remove_path(&entry.path()).await?;
    }

    let states = store.asset_states().await?;
    let assets_dir = data_dir.join("assets");
    let trash_dir = data_dir.join("trash");

    for directory in [&assets_dir, &trash_dir] {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let raw = entry.file_name();
            let Some(id) = raw.to_str().and_then(|name| AssetId::parse(name).ok()) else {
                tracing::warn!(entry = %raw.to_string_lossy(), "removing unrecognized entry from Atlas storage");
                remove_path(&entry.path()).await?;
                continue;
            };

            match states.get(id.as_str()) {
                None => remove_path(&entry.path()).await?,
                Some(AssetState::Purged) => remove_path(&entry.path()).await?,
                Some(_) => {}
            }
        }
    }

    for (id, state) in &states {
        let live = assets_dir.join(id);
        let trashed = trash_dir.join(id);
        let live_exists = tokio::fs::symlink_metadata(&live).await.is_ok();
        let trashed_exists = tokio::fs::symlink_metadata(&trashed).await.is_ok();

        match (state.is_retired(), live_exists, trashed_exists) {
            (true, false, true) | (false, true, false) => {}
            (true, true, trashed_exists) => {
                // Renaming onto an existing directory fails, so keep the copy
                // the row already points at.
                if trashed_exists {
                    remove_path(&live).await?;
                } else {
                    tokio::fs::rename(&live, &trashed).await?;
                    tracing::warn!(asset = %id, "completed an interrupted delete during recovery");
                }
            }
            (false, live_exists, true) => {
                if live_exists {
                    remove_path(&trashed).await?;
                } else {
                    tokio::fs::rename(&trashed, &live).await?;
                    tracing::warn!(asset = %id, "completed an interrupted restore during recovery");
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// The document served at `/api/v1/openapi.json`.
///
/// The paths and schemas come from the handlers registered with
/// `utoipa_axum::routes!`, so a route that is not annotated is not served:
/// the document cannot describe a route the router does not have, and the
/// router cannot have a route the document does not describe.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Atlas",
        version = env!("CARGO_PKG_VERSION"),
        description = "The HTTP API of an Atlas instance. This contract is pre-1.0: routes and responses may change, and the `api` number on /readyz marks a breaking change."
    ),
    security(("bearer" = [])),
    modifiers(&BearerAuth)
)]
struct ApiDoc;

/// Adds the bearer scheme the routes refer to.
struct BearerAuth;

impl utoipa::Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                utoipa::openapi::security::SecurityScheme::Http(
                    utoipa::openapi::security::HttpBuilder::new()
                        .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                        .build(),
                ),
            );
        }
    }
}

pub fn app(state: AppState) -> Router {
    let (router, mut document) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        // One `routes!` per path: handlers in a single call must share it, and
        // the macro folds their methods into one method router.
        .routes(routes!(api::health))
        .routes(routes!(api::ready))
        .routes(routes!(api::openapi))
        .routes(routes!(api::upload_image, api::list_images))
        .routes(routes!(api::upload_document, api::list_documents))
        .routes(routes!(api::get_asset, api::delete_asset))
        .routes(routes!(api::restore_asset))
        .routes(routes!(api::set_pin))
        .routes(routes!(api::rename_asset))
        .routes(routes!(api::set_data))
        .routes(routes!(api::bulk_actions))
        .routes(routes!(api::get_srcset))
        .routes(routes!(api::refresh_renditions))
        .routes(routes!(api::re_render_images))
        .routes(routes!(api::get_re_render_job))
        .routes(routes!(api::asset_states))
        .routes(routes!(api::assets_by_hash))
        .routes(routes!(api::summary))
        .routes(routes!(api::list_formats))
        .routes(routes!(api::list_trash))
        .routes(routes!(api::get_original))
        .routes(routes!(api::get_rendition))
        .routes(routes!(backup::create_backup, backup::list_backups))
        .routes(routes!(backup::download_backup, backup::delete_backup))
        .routes(routes!(backup::upload_restore))
        .routes(routes!(backup::apply_restore))
        .split_for_parts();
    add_live_examples(&mut document, &state);
    // The handlers read it back from the state, so a second `app()` on a clone
    // of the same state keeps the first document.
    state.openapi.set(document).ok();
    let router = router.with_state(state.clone());
    // Everything holds a read guard; the apply route itself bypasses this and
    // takes the write guard, so no request reads or writes the store while a
    // restore swaps its database and directories.
    router.layer(axum::middleware::from_fn_with_state(
        state,
        hold_restore_gate,
    ))
}

/// Block normal requests while a restore applies.
///
/// The apply route manages the write side itself, and backup creation takes
/// the read side inside its detached task. Both are exempt here so a
/// disconnect cannot drop a gate that the detached work still needs, and so a
/// fair writer cannot deadlock between the middleware and the task.
async fn hold_restore_gate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let is_apply = path.starts_with("/api/v1/backups/restore/") && path.ends_with("/apply");
    let is_backup_create = request.method() == Method::POST && path == "/api/v1/backups";
    if is_apply || is_backup_create {
        return next.run(request).await;
    }
    let _guard = state.restore_gate.read().await;
    next.run(request).await
}

/// Put the instance's own widths and formats into the document as examples.
///
/// The schemas stay static and per-build; only the examples are per-instance,
/// so a generator sees the values this deployment actually uses without the
/// contract becoming a description of one host.
fn add_live_examples(document: &mut utoipa::openapi::OpenApi, state: &AppState) {
    let widths = state.config.variant_widths.clone();
    let formats: Vec<&'static str> = model::RenditionFormat::ALL
        .iter()
        .map(|format| format.key())
        .collect();
    let (free_bytes, writable) = api::ready_disk(
        disk::space(&state.config.data_dir),
        state.config.min_free_bytes,
    );
    let Some(item) = document.paths.paths.get_mut("/readyz") else {
        return;
    };
    let Some(operation) = item.get.as_mut() else {
        return;
    };
    let Some(utoipa::openapi::RefOr::T(response)) = operation.responses.responses.get_mut("200")
    else {
        return;
    };
    if let Some(utoipa::openapi::RefOr::T(media)) = response.content.get_mut("application/json") {
        media.example = Some(serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "api": api::API_COMPATIBILITY,
            "rendition_widths": widths,
            "formats": formats,
            "writable": writable,
            "free_bytes": free_bytes,
        }));
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::api::health;
    use axum::{Router, routing::get};

    #[tokio::test]
    async fn health_endpoint_returns_json_ok() {
        let response = Router::new()
            .route("/healthz", get(health))
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }
}

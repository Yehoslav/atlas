mod api;
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

use axum::Router;
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
    /// How many async uploads may convert or wait for a conversion permit at
    /// once: as many as there are permits. The rest of the backlog waits
    /// here, so a synchronous upload or a re-render queues behind at most
    /// one conversion per permit, not behind every accepted upload.
    pub(crate) upload_job_slots: Arc<Semaphore>,
    asset_conversion_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    /// Serializes accepting an async upload against the queue cap and
    /// promoting one against the hash check. Both are a read and a write that
    /// must not interleave with another of their kind. Their transactions are
    /// immediate, so SQLite serializes them too; the lock makes this
    /// process's own jobs queue here rather than on the busy timeout.
    pub(crate) upload_jobs_lock: Arc<Mutex<()>>,
    /// How many accepted async uploads may wait to convert at once.
    pub(crate) max_pending_uploads: usize,
    pub(crate) last_purge: Arc<Mutex<Option<Instant>>>,
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
        create_private_dir(&config.data_dir.join("uploads"), true).await?;

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
            upload_job_slots: Arc::new(Semaphore::new(conversion_concurrency)),
            asset_conversion_locks: Arc::new(Mutex::new(HashMap::new())),
            upload_jobs_lock: Arc::new(Mutex::new(())),
            max_pending_uploads: api::MAX_PENDING_UPLOADS,
            last_purge: Arc::new(Mutex::new(None)),
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
        api::resume_upload_jobs(state.clone()).await;

        Ok(state)
    }

    pub(crate) fn asset_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("assets").join(id.as_str())
    }

    pub(crate) fn staging_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("staging").join(id.as_str())
    }

    /// Where an accepted async upload keeps its original until the job is
    /// terminal. Outside `staging/`, which startup empties, so a crash
    /// mid-conversion converts the same bytes again.
    pub(crate) fn upload_job_dir(&self, job_id: &str) -> PathBuf {
        self.config.data_dir.join("uploads").join(job_id)
    }

    pub(crate) fn trash_dir(&self, id: &AssetId) -> PathBuf {
        self.config.data_dir.join("trash").join(id.as_str())
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
/// work. An async upload's original survives only while its job is still
/// converting, which is what lets the job resume. Anything else is moved back to where its row says it belongs, so an
/// interrupted delete or restore converges. The trash itself is left alone: a
/// trashed asset is a deliberate state, not a crash artifact, and only the
/// retention sweep discards it.
async fn recover_incomplete_storage(
    data_dir: &Path,
    store: &Store,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let staging_dir = data_dir.join("staging");
    let mut entries = tokio::fs::read_dir(&staging_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        remove_path(&entry.path()).await?;
    }

    // An upload waiting to convert keeps its original; anything else here is
    // left over from a job that already finished, or from an intake that
    // crashed between moving its bytes and writing its row.
    let converting: std::collections::HashSet<String> = store
        .converting_upload_jobs()
        .await?
        .into_iter()
        .map(|job| job.id)
        .collect();
    let mut entries = tokio::fs::read_dir(data_dir.join("uploads")).await?;
    while let Some(entry) = entries.next_entry().await? {
        let keep = entry
            .file_name()
            .to_str()
            .is_some_and(|name| converting.contains(name));
        if !keep {
            remove_path(&entry.path()).await?;
        }
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
        .routes(routes!(api::get_upload_job))
        .routes(routes!(api::asset_states))
        .routes(routes!(api::assets_by_hash))
        .routes(routes!(api::summary))
        .routes(routes!(api::list_formats))
        .routes(routes!(api::list_trash))
        .routes(routes!(api::get_original))
        .routes(routes!(api::get_rendition))
        .split_for_parts();
    add_live_examples(&mut document, &state);
    // The handlers read it back from the state, so a second `app()` on a clone
    // of the same state keeps the first document.
    state.openapi.set(document).ok();
    router.with_state(state)
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

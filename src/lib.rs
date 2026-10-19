mod api;
pub mod config;
mod error;
mod filename;
mod ident;
mod media;
mod model;
mod store;

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{Router, routing::get};
use ident::AssetId;
use media::ImageProcessor;
use model::AssetState;
use store::Store;
use tokio::sync::{Mutex, Semaphore};

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
    pub(crate) last_purge: Arc<Mutex<Option<Instant>>>,
}

impl AppState {
    pub async fn new(mut config: Config) -> Result<Self, Box<dyn Error + Send + Sync>> {
        create_private_dir(&config.data_dir, false).await?;
        config.data_dir = tokio::fs::canonicalize(&config.data_dir).await?;
        create_private_dir(&config.data_dir.join("staging"), true).await?;
        create_private_dir(&config.data_dir.join("assets"), true).await?;
        create_private_dir(&config.data_dir.join("trash"), true).await?;

        let store = Store::connect(&config.data_dir.join("atlas.sqlite3")).await?;
        recover_incomplete_storage(&config.data_dir, &store).await?;
        let image_processor = ImageProcessor::new(
            config.magick_bin.clone(),
            config.max_image_pixels,
            config.max_image_dimension,
            config.magick_timeout,
        );
        image_processor.verify_formats().await?;

        let state = Self {
            config: Arc::new(config),
            store,
            image_processor,
            upload_slots: Arc::new(Semaphore::new(2)),
            conversion_slots: Arc::new(Semaphore::new(1)),
            last_purge: Arc::new(Mutex::new(None)),
        };

        match state.purge_expired_trash().await {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "purged expired trashed assets at startup"),
            Err(error) => tracing::error!(%error, "startup trash purge failed"),
        }

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
/// work. Anything else is moved back to where its row says it belongs, so an
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

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(api::health))
        // Two collections: the caller's choice is in the path, so asking for a
        // rendition is asking for an image by construction.
        .route(
            "/api/v1/images",
            axum::routing::post(api::upload_image).get(api::list_images),
        )
        .route(
            "/api/v1/documents",
            axum::routing::post(api::upload_document).get(api::list_documents),
        )
        .route(
            "/api/v1/{kind}/{id}",
            get(api::get_asset).delete(api::delete_asset),
        )
        .route(
            "/api/v1/{kind}/{id}/restore",
            axum::routing::post(api::restore_asset),
        )
        .route("/api/v1/{kind}/{id}/pin", axum::routing::put(api::set_pin))
        // A consumer's own opaque note. Typed like the rest: it belongs to an
        // asset, and an asset belongs to a collection.
        .route(
            "/api/v1/{kind}/{id}/data",
            axum::routing::put(api::set_data),
        )
        .route(
            "/api/v1/{kind}/actions",
            axum::routing::post(api::bulk_actions),
        )
        // Images only: documents have no renditions, so there is no such route.
        .route("/api/v1/images/{id}/srcset", get(api::get_srcset))
        // Uniform reads: a caller naturally asks about a mixed set of ids, and
        // reading is harmless when the caller's classification is wrong.
        .route(
            "/api/v1/assets/state",
            axum::routing::post(api::asset_states),
        )
        .route("/api/v1/summary", get(api::summary))
        .route("/api/v1/formats", get(api::list_formats))
        .route("/api/v1/trash", get(api::list_trash))
        // Public delivery is addressed by id and stays untyped.
        .route("/media/{id}/original", get(api::get_original))
        .route("/media/{id}/{rendition}", get(api::get_rendition))
        .with_state(state)
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

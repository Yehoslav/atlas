mod api;
pub mod config;
mod error;
mod ident;
mod media;
mod model;
mod store;

use std::sync::Arc;

use axum::{Router, routing::get};
use ident::AssetId;
use media::ImageProcessor;
use store::Store;
use tokio::sync::Semaphore;

pub use config::Config;
pub use ident::AssetId as PublicAssetId;

#[derive(Clone)]
pub struct AppState {
    pub(crate) config: Arc<Config>,
    pub(crate) store: Store,
    pub(crate) image_processor: ImageProcessor,
    pub(crate) upload_slots: Arc<Semaphore>,
    pub(crate) conversion_slots: Arc<Semaphore>,
}

impl AppState {
    pub async fn new(mut config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        create_private_dir(&config.data_dir, false).await?;
        config.data_dir = tokio::fs::canonicalize(&config.data_dir).await?;
        create_private_dir(&config.data_dir.join("staging"), true).await?;
        create_private_dir(&config.data_dir.join("assets"), true).await?;

        let store = Store::connect(&config.data_dir.join("atlas.sqlite3")).await?;
        recover_incomplete_storage(&config.data_dir, &store).await?;
        let image_processor = ImageProcessor::new(
            config.magick_bin.clone(),
            config.max_image_pixels,
            config.max_image_dimension,
            config.magick_timeout,
        );
        image_processor.verify_formats().await?;

        Ok(Self {
            config: Arc::new(config),
            store,
            image_processor,
            upload_slots: Arc::new(Semaphore::new(2)),
            conversion_slots: Arc::new(Semaphore::new(1)),
        })
    }

    pub(crate) fn asset_dir(&self, id: &AssetId) -> std::path::PathBuf {
        self.config.data_dir.join("assets").join(id.as_str())
    }

    pub(crate) fn staging_dir(&self, id: &AssetId) -> std::path::PathBuf {
        self.config.data_dir.join("staging").join(id.as_str())
    }
}

async fn create_private_dir(
    path: &std::path::Path,
    allow_tighten_existing: bool,
) -> std::io::Result<()> {
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

async fn recover_incomplete_storage(
    data_dir: &std::path::Path,
    store: &Store,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let staging_dir = data_dir.join("staging");
    let mut entries = tokio::fs::read_dir(&staging_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }

    let known_assets = store.asset_ids().await?;
    let assets_dir = data_dir.join("assets");
    let mut entries = tokio::fs::read_dir(assets_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !known_assets.contains(name.as_ref()) {
            if entry.file_type().await?.is_dir() {
                tokio::fs::remove_dir_all(entry.path()).await?;
            } else {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
    }

    Ok(())
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(api::health))
        .route("/api/v1/assets", axum::routing::post(api::upload_asset))
        .route("/api/v1/assets/{id}", get(api::get_asset))
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

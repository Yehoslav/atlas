//! Backup bundles and restore validation.
//!
//! A bundle pairs a `VACUUM INTO` snapshot of the metadata database with the
//! `assets/` and `trash/` bytes. The two are useless separately: a database
//! restored beside a different set of asset directories names files that are
//! not there. The archive is a plain, uncompressed tar — the media bytes are
//! already compressed, and a streamable format with a manifest is easier to
//! verify than a compressed one.
//!
//! Restore uploads are untrusted input, exactly like media uploads: they are
//! streamed to a staging file under a byte limit, extracted with an allowlist
//! and path checks, and validated before anything live is touched.

use std::{
    collections::HashSet,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path as AxumPath, Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, EntryType, Header, HeaderMode};
use tokio::{fs as async_fs, io::AsyncWriteExt};

use crate::{AppState, api, disk, error::ApiError, ident::AssetId, media::sha256_file};

/// The only bundle format this build writes or accepts.
const FORMAT_VERSION: u32 = 1;
const MANIFEST_MEMBER: &str = "manifest.json";
const DATABASE_MEMBER: &str = "atlas.sqlite3";
/// Kept in the data directory root, not under `staging/`, because startup must
/// see it before it opens a database or sweeps staging.
const RESTORE_JOURNAL: &str = "restore.journal";
const ASSETS_MEMBER: &str = "assets";
const TRASH_MEMBER: &str = "trash";

/// A backup costs roughly the data directory, and both backing up and
/// restoring write a second copy. Up to 200,000 members is far beyond a small
/// site's library while still bounding the metadata a hostile archive can make
/// Atlas allocate.
const MAX_ARCHIVE_MEMBERS: usize = 200_000;
/// The manifest is a small fixed description, not a place for arbitrary data.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

const BACKUP_CACHE_POLICY: &str = "private, no-store";

/// The manifest stored as the first-class description of a bundle.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct RestoreJournal {
    upload_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(crate) struct BackupManifest {
    pub format_version: u32,
    /// Unix seconds at the `VACUUM INTO` cut.
    pub created_at: i64,
    /// The newest applied migration in the snapshot.
    pub schema_version: i64,
    pub counts: BackupCounts,
    /// SHA-256 of `atlas.sqlite3` inside the archive.
    pub database_sha256: String,
    /// Informational facts about the instance that made the bundle.
    pub instance: InstanceFacts,
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(crate) struct BackupCounts {
    pub assets: i64,
    pub renditions: i64,
    pub superseded_renditions: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(crate) struct InstanceFacts {
    pub id_length: u32,
    pub rendition_widths: Vec<u32>,
    pub accepted_formats: Vec<String>,
}

/// One stored bundle.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct BackupInfo {
    pub id: String,
    pub bytes: u64,
    pub created_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct BackupsResponse {
    pub backups: Vec<BackupInfo>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct CreatedBackup {
    pub id: String,
    pub bytes: u64,
    pub created_at: i64,
    pub manifest: BackupManifest,
}

/// What validation learned, and the warning an operator must see before
/// applying. This is also written beside the extracted tree, so apply does not
/// trust the connection that uploaded it.
#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(crate) struct RestoreReport {
    pub upload_id: String,
    pub created_at: i64,
    pub schema_version: i64,
    pub counts: BackupCounts,
    pub database_sha256: String,
    pub manifest: BackupManifest,
    /// Where this instance differs from the instance that made the bundle.
    /// Informational: the restore still applies.
    pub configuration_differences: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AppliedRestore {
    pub upload_id: String,
    pub counts: BackupCounts,
    pub warnings: Vec<String>,
}

/// `POST /api/v1/backups`.
#[utoipa::path(
    post,
    path = "/api/v1/backups",
    responses((status = 201, description = "A consistent bundle was written.", body = CreatedBackup),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse),
        (status = 409, description = "Another backup is already running.", body = crate::error::ErrorResponse),
        (status = 507, description = "The data volume is below the free-space floor.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn create_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<CreatedBackup>), ApiError> {
    api::authorize(&state, &headers)?;
    // Detach the work from this connection. A client that disconnects must not
    // drop the backup or restore gate while the copy is still running.
    tokio::spawn(async move {
        let _gate = state.restore_gate.read().await;
        create(&state).await
    })
    .await
    .map_err(ApiError::internal)?
}

async fn create(state: &AppState) -> Result<(StatusCode, Json<CreatedBackup>), ApiError> {
    // A second backup would double the disk cost and race the purge pause.
    let _backup_guard = state.backup_lock.clone().try_lock_owned().map_err(|_| {
        ApiError::conflict(
            "backup_in_progress",
            "A backup or trash purge is already running; wait for it to finish.",
        )
    })?;

    // Keep delete and restore-from-trash out of the snapshot-and-copy window.
    let _lifecycle = state.lifecycle_lock.write().await;

    ensure_backup_space(state).await?;

    let created_at = crate::unix_now().map_err(ApiError::internal)?;
    let id = random_id().map_err(ApiError::internal)?;
    let staging = state.backup_staging_dir();
    async_fs::create_dir_all(&staging)
        .await
        .map_err(ApiError::internal)?;

    let snapshot = staging.join(format!("{id}.sqlite3"));
    let tar_path = staging.join(format!("{id}.tar"));
    let result = create_inner(state, &id, created_at, &snapshot, &tar_path).await;
    if result.is_err() {
        let _ = async_fs::remove_file(&snapshot).await;
        let _ = async_fs::remove_file(&tar_path).await;
    }
    let created = result?;

    let final_path = state.backup_path(&id);
    async_fs::rename(&tar_path, &final_path)
        .await
        .map_err(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn create_inner(
    state: &AppState,
    id: &str,
    created_at: i64,
    snapshot: &Path,
    tar_path: &Path,
) -> Result<CreatedBackup, ApiError> {
    let _ = async_fs::remove_file(snapshot).await;
    // The snapshot defines the backup's time cut and comes first, as the
    // runbook requires: files copied afterwards may be newer, which is
    // harmless, but never newer than the database in a way that loses one it
    // names.
    state
        .store
        .snapshot_to(snapshot)
        .await
        .map_err(ApiError::internal)?;
    let inspection = crate::store::Store::inspect_snapshot(snapshot)
        .await
        .map_err(ApiError::internal)?;
    let database_sha256 = sha256_file(snapshot).await.map_err(ApiError::internal)?;

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        created_at,
        schema_version: inspection.schema_version,
        counts: BackupCounts {
            assets: inspection.assets,
            renditions: inspection.renditions,
            superseded_renditions: inspection.superseded_renditions,
        },
        database_sha256: database_sha256.clone(),
        instance: InstanceFacts {
            id_length: state.config.id_length as u32,
            rendition_widths: state.config.variant_widths.clone(),
            accepted_formats: crate::model::SourceFormat::ALL
                .iter()
                .map(|format| format.mime_type().to_owned())
                .collect(),
        },
    };

    let data_dir = state.config.data_dir.clone();
    let snapshot_for_tar = snapshot.to_owned();
    let tar_path = tar_path.to_owned();
    let live_ids = inspection.live_asset_ids.clone();
    let trashed_ids = inspection.trashed_asset_ids.clone();
    let manifest_for_tar = manifest.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        write_bundle(
            &data_dir,
            &snapshot_for_tar,
            &tar_path,
            &manifest_for_tar,
            &live_ids,
            &trashed_ids,
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    let _ = async_fs::remove_file(&snapshot).await;
    Ok(CreatedBackup {
        id: id.to_owned(),
        bytes,
        created_at,
        manifest,
    })
}

/// `GET /api/v1/backups`.
#[utoipa::path(
    get,
    path = "/api/v1/backups",
    responses((status = 200, description = "Stored bundles.", body = BackupsResponse),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn list_backups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<BackupsResponse>, ApiError> {
    api::authorize(&state, &headers)?;
    let mut backups = Vec::new();
    let mut entries = async_fs::read_dir(state.backups_dir())
        .await
        .map_err(ApiError::internal)?;
    while let Some(entry) = entries.next_entry().await.map_err(ApiError::internal)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".tar").filter(|id| is_short_id(id)) else {
            continue;
        };
        let metadata = match entry.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        backups.push(BackupInfo {
            id: id.to_owned(),
            bytes: metadata.len(),
            created_at: unix_seconds(metadata.modified().ok()),
        });
    }
    backups.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(Json(BackupsResponse { backups }))
}

/// `GET /api/v1/backups/{id}/archive`.
#[utoipa::path(
    get,
    path = "/api/v1/backups/{id}/archive",
    params(("id" = String, Path, description = "The backup id.")),
    responses((status = 200, description = "The bundle, streamed from disk.", content_type = "application/x-tar"),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse),
        (status = 404, description = "No such backup.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    api::authorize(&state, &headers)?;
    let id = parse_backup_id(&id)?;
    let path = state.backup_path(&id);
    let metadata = async_fs::metadata(&path).await.map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            backup_not_found()
        } else {
            ApiError::internal(error)
        }
    })?;
    if !metadata.is_file() {
        return Err(backup_not_found());
    }
    let created_at = unix_seconds(metadata.modified().ok());
    let filename = format!("atlas-backup-{}.tar", utc_stamp(created_at));
    let etag = format!("\"{}-{created_at}\"", metadata.len());
    api::serve_file(
        &headers,
        &path,
        metadata.len(),
        &etag,
        Some(&filename),
        "application/x-tar",
        "attachment",
        false,
        BACKUP_CACHE_POLICY,
    )
    .await
}

/// `DELETE /api/v1/backups/{id}`.
#[utoipa::path(
    delete,
    path = "/api/v1/backups/{id}",
    params(("id" = String, Path, description = "The backup id.")),
    responses((status = 204, description = "The bundle was removed."),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse),
        (status = 404, description = "No such backup.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn delete_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    api::authorize(&state, &headers)?;
    let id = parse_backup_id(&id)?;
    async_fs::remove_file(state.backup_path(&id))
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                backup_not_found()
            } else {
                ApiError::internal(error)
            }
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/backups/restore` — upload and validate, but touch nothing live.
#[utoipa::path(
    post,
    path = "/api/v1/backups/restore",
    request_body(content(("application/x-tar")), description = "A backup bundle."),
    responses((status = 200, description = "The bundle is valid and ready to apply.", body = RestoreReport),
        (status = 400, description = "Invalid request.", body = crate::error::ErrorResponse),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse),
        (status = 413, description = "The upload exceeds the configured limit.", body = crate::error::ErrorResponse),
        (status = 415, description = "Unsupported media type.", body = crate::error::ErrorResponse),
        (status = 422, description = "The bundle is not valid or cannot be restored.", body = crate::error::ErrorResponse),
        (status = 507, description = "The data volume is below the free-space floor.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn upload_restore(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<RestoreReport>, ApiError> {
    api::authorize(&state, request.headers())?;
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type
        .split(';')
        .next()
        .is_some_and(|value| !value.trim().eq_ignore_ascii_case("application/x-tar"))
    {
        return Err(ApiError::unsupported(
            "Restore uploads must be application/x-tar.",
        ));
    }
    if let Some(encoding) = request.headers().get(CONTENT_ENCODING)
        && !encoding
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
    {
        return Err(ApiError::unsupported(
            "Compressed request bodies are not supported.",
        ));
    }

    let limit = state.config.backup_upload_max_bytes;
    if let Some(content_length) = request.headers().get(CONTENT_LENGTH) {
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
        ensure_restore_space(&state, declared)?;
    }

    let upload_id = random_id().map_err(ApiError::internal)?;
    let directory = state.restore_staging_dir(&upload_id);
    async_fs::create_dir_all(&directory)
        .await
        .map_err(ApiError::internal)?;
    let tar_path = directory.join("upload.tar");
    let result = async {
        let (bytes, _) = api::stream_upload(request.into_body(), &tar_path, limit).await?;
        ensure_restore_space(&state, bytes)?;
        validate(&state, &upload_id, &directory, &tar_path).await
    }
    .await;
    if result.is_err() {
        let _ = async_fs::remove_dir_all(&directory).await;
    }
    Ok(Json(result?))
}

/// `POST /api/v1/backups/restore/{upload_id}/apply` — the swap.
#[utoipa::path(
    post,
    path = "/api/v1/backups/restore/{upload_id}/apply",
    params(("upload_id" = String, Path, description = "The validated upload id.")),
    responses((status = 200, description = "The bundle was applied.", body = AppliedRestore),
        (status = 401, description = "Missing or invalid token.", body = crate::error::ErrorResponse),
        (status = 404, description = "No validated upload with that id.", body = crate::error::ErrorResponse),
        (status = 422, description = "The staged upload no longer validates.", body = crate::error::ErrorResponse),
        (status = 500, description = "The swap failed; the live instance was restored.", body = crate::error::ErrorResponse))
)]
pub(crate) async fn apply_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(upload_id): AxumPath<String>,
) -> Result<Json<AppliedRestore>, ApiError> {
    api::authorize(&state, &headers)?;
    let upload_id = parse_upload_id(&upload_id)?;
    // Detach the swap from the connection: a disconnect must not release the
    // write gate or abort the file moves halfway through.
    tokio::spawn(async move {
        // The request layer already holds a read guard for everything except
        // this route; taking the write guard here waits for every in-flight
        // request.
        let _gate = state.restore_gate.write().await;
        apply_inner(&state, &upload_id).await
    })
    .await
    .map_err(ApiError::internal)?
}

async fn apply_inner(state: &AppState, upload_id: &str) -> Result<Json<AppliedRestore>, ApiError> {
    let directory = state.restore_staging_dir(upload_id);
    let tree = directory.join("tree");
    let report_path = directory.join("report.json");
    let report_bytes = async_fs::read(&report_path).await.map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            restore_not_found()
        } else {
            ApiError::internal(error)
        }
    })?;
    let report: RestoreReport =
        serde_json::from_slice(&report_bytes).map_err(ApiError::internal)?;

    // Recheck the database and the cross-reference; staging is private, but a
    // restore is the one operation that replaces the whole instance, so the
    // cheap checks run again rather than trusting a file written earlier.
    let database = tree.join(DATABASE_MEMBER);
    let inspection = crate::store::Store::inspect_snapshot(&database)
        .await
        .map_err(|error| invalid_backup(format!("The staged database is invalid: {error}")))?;
    let actual_sha256 = sha256_file(&database).await.map_err(ApiError::internal)?;
    if actual_sha256 != report.database_sha256 {
        return Err(invalid_backup(
            "The staged database no longer matches the validated upload.",
        ));
    }
    verify_directories(
        &tree,
        &inspection.live_asset_ids,
        &inspection.trashed_asset_ids,
    )?;

    let data_dir = state.config.data_dir.clone();
    let live_db = data_dir.join(DATABASE_MEMBER);
    let rollback = directory.join("rollback");
    async_fs::create_dir_all(&rollback)
        .await
        .map_err(ApiError::internal)?;

    // From here on a crash is recoverable only if startup can tell that a
    // swap was in progress. The journal is written and fsynced before any
    // live path moves, and removed only after the new database is open and
    // verified.
    write_journal(&data_dir, upload_id)
        .await
        .map_err(ApiError::internal)?;

    // Stop the pool before moving the database file: an open SQLite handle
    // with a `-wal` beside it would otherwise follow the wrong file.
    state.store.close_pool().await;

    let swapped = swap_in(&data_dir, &tree, &rollback, &live_db).await;
    if let Err(error) = swapped {
        if let Err(rollback_error) = swap_back(&data_dir, &rollback, &live_db).await {
            tracing::error!(
                ?rollback_error,
                "restore swap failed and rollback also failed; leaving the journal for startup"
            );
        } else {
            let _ = remove_journal(&data_dir).await;
        }
        if let Err(reload_error) = state.store.reload(&live_db).await {
            tracing::error!(%reload_error, "could not reopen the old database after a failed restore");
        }
        return Err(error);
    }

    if let Err(error) = state.store.reload(&live_db).await {
        tracing::error!(%error, "restored database failed to open; rolling the swap back");
        if let Err(rollback_error) = swap_back(&data_dir, &rollback, &live_db).await {
            tracing::error!(
                ?rollback_error,
                "restore rollback failed; live instance needs manual recovery"
            );
            return Err(ApiError::internal(
                "The restored database failed and the previous files could not be restored; stop Atlas and recover from the deployment runbook.",
            ));
        }
        let _ = remove_journal(&data_dir).await;
        if let Err(reload_error) = state.store.reload(&live_db).await {
            tracing::error!(%reload_error, "could not reopen the old database after rollback");
        }
        return Err(invalid_backup(format!(
            "The restored database could not be opened: {error}"
        )));
    }

    // The new database opened and passed its checks. Removing the journal is
    // the commit point: from here a restart keeps the restored instance.
    if let Err(error) = remove_journal(&data_dir).await {
        tracing::error!(%error, "could not commit the restore journal; rolling back");
        if let Err(rollback_error) = swap_back(&data_dir, &rollback, &live_db).await {
            tracing::error!(
                ?rollback_error,
                "restore rollback failed after journal commit error"
            );
            return Err(ApiError::internal(
                "The restore could not be committed and the previous files could not be restored; stop Atlas and recover from the deployment runbook.",
            ));
        }
        let _ = remove_journal(&data_dir).await;
        if let Err(reload_error) = state.store.reload(&live_db).await {
            tracing::error!(%reload_error, "could not reopen the old database after a commit failure");
        }
        return Err(ApiError::internal(
            "The restore could not be committed; the previous instance was restored.",
        ));
    }

    // The new rows and directories are live. Startup recovery reconciles
    // unindexed directories and interrupted lifecycle states, and clears the
    // staging tree this restore came from.
    if let Err(error) = crate::recover_incomplete_storage(&data_dir, &state.store).await {
        tracing::error!(%error, "post-restore recovery failed; the service is serving the restored snapshot");
    }

    tracing::info!(
        upload = %upload_id,
        assets = report.counts.assets,
        "applied a restore"
    );
    Ok(Json(AppliedRestore {
        upload_id: upload_id.to_owned(),
        counts: report.counts.clone(),
        warnings: report.warnings,
    }))
}

/// Move the live directories and database aside, then move the staged ones
/// in. The staged database rename is the last step and the commit point; the
/// journal still exists until the new database has been opened and checked, so
/// a crash anywhere in this sequence is rolled back at startup.
async fn swap_in(
    data_dir: &Path,
    tree: &Path,
    rollback: &Path,
    live_db: &Path,
) -> Result<(), ApiError> {
    for name in [ASSETS_MEMBER, TRASH_MEMBER] {
        let live = data_dir.join(name);
        let saved = rollback.join(name);
        if async_fs::symlink_metadata(&live).await.is_ok() {
            async_fs::rename(&live, &saved)
                .await
                .map_err(ApiError::internal)?;
        }
        let staged = tree.join(name);
        if async_fs::symlink_metadata(&staged).await.is_ok() {
            async_fs::rename(&staged, &live)
                .await
                .map_err(ApiError::internal)?;
        } else {
            async_fs::create_dir_all(&live)
                .await
                .map_err(ApiError::internal)?;
        }
    }

    // The old database goes aside after the directories, so if a crash
    // happens before this point the live database is still the old one and
    // the journal can simply restore the directories.
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", live_db.display()));
        if async_fs::symlink_metadata(&from).await.is_ok() {
            let to = rollback.join(format!("atlas.sqlite3{suffix}"));
            async_fs::rename(&from, &to)
                .await
                .map_err(ApiError::internal)?;
        }
    }
    // Commit point: the database that matches the directories just moved in.
    async_fs::rename(tree.join(DATABASE_MEMBER), live_db)
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

/// Put the previous database and directories back after a failed swap.
async fn swap_back(data_dir: &Path, rollback: &Path, live_db: &Path) -> Result<(), ApiError> {
    swap_back_io(data_dir, rollback, live_db)
        .await
        .map_err(ApiError::internal)
}

async fn swap_back_io(data_dir: &Path, rollback: &Path, live_db: &Path) -> io::Result<()> {
    // Only touch the live database if the swap actually moved it aside. A
    // failure before that point leaves the old file where it is.
    let saved_main = rollback.join("atlas.sqlite3");
    if async_fs::symlink_metadata(&saved_main).await.is_ok() {
        // Remove whatever database the failed attempt opened, including a WAL
        // the old snapshot did not have; restoring the old `-wal`/`-shm`
        // beside a different main file would be worse than having none.
        for suffix in ["", "-wal", "-shm"] {
            let live = PathBuf::from(format!("{}{suffix}", live_db.display()));
            let _ = async_fs::remove_file(&live).await;
        }
        for suffix in ["", "-wal", "-shm"] {
            let saved = rollback.join(format!("atlas.sqlite3{suffix}"));
            if async_fs::symlink_metadata(&saved).await.is_ok() {
                let live = PathBuf::from(format!("{}{suffix}", live_db.display()));
                async_fs::rename(&saved, &live).await?;
            }
        }
    }
    for name in [ASSETS_MEMBER, TRASH_MEMBER] {
        let live = data_dir.join(name);
        let saved = rollback.join(name);
        if async_fs::symlink_metadata(&saved).await.is_ok() {
            // The staged directory is live; discard it before restoring the
            // previous one under the same name.
            if async_fs::symlink_metadata(&live).await.is_ok() {
                async_fs::remove_dir_all(&live).await?;
            }
            async_fs::rename(&saved, &live).await?;
        }
    }
    Ok(())
}

fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RESTORE_JOURNAL)
}

async fn write_journal(data_dir: &Path, upload_id: &str) -> io::Result<()> {
    let bytes = serde_json::to_vec(&RestoreJournal {
        upload_id: upload_id.to_owned(),
    })
    .map_err(io::Error::other)?;
    let path = journal_path(data_dir);
    let mut file = async_fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    sync_dir(data_dir).await
}

async fn remove_journal(data_dir: &Path) -> io::Result<()> {
    match async_fs::remove_file(journal_path(data_dir)).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sync_dir(data_dir).await
}

async fn sync_dir(path: &Path) -> io::Result<()> {
    async_fs::File::open(path).await?.sync_all().await
}

/// Finish or undo a swap that was interrupted by a crash, before the database
/// is opened and before staging is swept.
///
/// The journal is written before any live path moves and removed only after
/// the new database has been opened and checked. Its presence therefore means
/// "roll back": the previous database and directories are still under the
/// upload's `rollback/` directory. If nothing was moved, the live files are
/// already correct and only the journal is discarded.
pub(crate) async fn recover_interrupted_restore(
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = journal_path(data_dir);
    let bytes = match async_fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Box::new(error)),
    };
    let journal: RestoreJournal = serde_json::from_slice(&bytes)
        .map_err(|error| format!("restore journal {} is invalid: {error}", path.display()))?;
    if !is_short_id(&journal.upload_id) {
        return Err(format!(
            "restore journal names an invalid upload id: {}",
            journal.upload_id
        )
        .into());
    }

    let rollback = data_dir
        .join("staging")
        .join("restores")
        .join(&journal.upload_id)
        .join("rollback");
    let live_db = data_dir.join(DATABASE_MEMBER);

    tracing::warn!(
        upload = %journal.upload_id,
        "found an unfinished restore; rolling the swap back"
    );
    if async_fs::symlink_metadata(&rollback).await.is_ok() {
        swap_back_io(data_dir, &rollback, &live_db).await?;
    }
    if async_fs::symlink_metadata(&live_db).await.is_err() {
        return Err(
            "an unfinished restore left no live database and no rollback copy; recover from the deployment runbook"
                .into(),
        );
    }
    remove_journal(data_dir).await?;
    tracing::warn!(
        upload = %journal.upload_id,
        "rolled back the unfinished restore"
    );
    Ok(())
}

async fn validate(
    state: &AppState,
    upload_id: &str,
    directory: &Path,
    tar_path: &Path,
) -> Result<RestoreReport, ApiError> {
    let tree = directory.join("tree");
    if async_fs::symlink_metadata(&tree).await.is_ok() {
        async_fs::remove_dir_all(&tree)
            .await
            .map_err(ApiError::internal)?;
    }

    let tar_path_owned = tar_path.to_owned();
    let tree_owned = tree.clone();
    let tar_size = async_fs::metadata(tar_path)
        .await
        .map_err(ApiError::internal)?
        .len();
    tokio::task::spawn_blocking(move || extract_bundle(&tar_path_owned, &tree_owned, tar_size))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| {
            invalid_backup(format!("The restore archive is not safe to apply: {error}"))
        })?;

    let manifest_path = tree.join(MANIFEST_MEMBER);
    let manifest_size = async_fs::metadata(&manifest_path)
        .await
        .map_err(|_| invalid_backup("The archive has no manifest.json."))?
        .len();
    if manifest_size > MAX_MANIFEST_BYTES {
        return Err(invalid_backup(format!(
            "manifest.json is {manifest_size} bytes; the limit is {MAX_MANIFEST_BYTES}."
        )));
    }
    let manifest_bytes = async_fs::read(&manifest_path)
        .await
        .map_err(|_| invalid_backup("The archive has no manifest.json."))?;
    let manifest: BackupManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| invalid_backup(format!("manifest.json is invalid: {error}")))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(invalid_backup(format!(
            "The bundle format version is {}; this Atlas reads version {FORMAT_VERSION}.",
            manifest.format_version
        )));
    }

    let database = tree.join(DATABASE_MEMBER);
    let actual_sha256 = sha256_file(&database).await.map_err(ApiError::internal)?;
    if actual_sha256 != manifest.database_sha256 {
        return Err(invalid_backup(
            "The database snapshot does not match the SHA-256 in the manifest.",
        ));
    }

    let inspection = crate::store::Store::inspect_snapshot(&database)
        .await
        .map_err(|error| invalid_backup(format!("The database snapshot is not usable: {error}")))?;
    let latest = crate::store::latest_schema_version();
    if inspection.schema_version > latest {
        return Err(ApiError::conflict(
            "backup_schema_too_new",
            format!(
                "The bundle's schema version {} is newer than this Atlas supports ({latest}); upgrade Atlas before restoring it.",
                inspection.schema_version
            ),
        ));
    }
    if inspection.assets != manifest.counts.assets
        || inspection.renditions != manifest.counts.renditions
        || inspection.superseded_renditions != manifest.counts.superseded_renditions
    {
        return Err(invalid_backup(
            "The database row counts do not match the manifest.",
        ));
    }
    verify_directories(
        &tree,
        &inspection.live_asset_ids,
        &inspection.trashed_asset_ids,
    )?;

    let configuration_differences = configuration_differences(&manifest.instance, state);
    let warnings = vec![
        "Applying this restore replaces the live database and all asset files; assets and \
         edits created after the bundle's snapshot will be lost."
            .to_owned(),
    ];

    let report = RestoreReport {
        upload_id: upload_id.to_owned(),
        created_at: manifest.created_at,
        schema_version: inspection.schema_version,
        counts: manifest.counts.clone(),
        database_sha256: actual_sha256,
        manifest,
        configuration_differences,
        warnings,
    };
    let report_bytes = serde_json::to_vec_pretty(&report).map_err(ApiError::internal)?;
    async_fs::write(directory.join("report.json"), report_bytes)
        .await
        .map_err(ApiError::internal)?;
    // The archive is not needed after extraction. Keep the tree and report,
    // which are what apply uses, and free the duplicate bytes.
    let _ = async_fs::remove_file(tar_path).await;
    Ok(report)
}

fn configuration_differences(instance: &InstanceFacts, state: &AppState) -> Vec<String> {
    let mut differences = Vec::new();
    if instance.id_length != state.config.id_length as u32 {
        differences.push(format!(
            "id_length: bundle {}, this instance {}",
            instance.id_length, state.config.id_length
        ));
    }
    if instance.rendition_widths != state.config.variant_widths {
        differences.push(format!(
            "rendition_widths: bundle {:?}, this instance {:?}",
            instance.rendition_widths, state.config.variant_widths
        ));
    }
    let formats: Vec<String> = crate::model::SourceFormat::ALL
        .iter()
        .map(|format| format.mime_type().to_owned())
        .collect();
    if instance.accepted_formats != formats {
        differences.push(format!(
            "accepted_formats: bundle {:?}, this instance {:?}",
            instance.accepted_formats, formats
        ));
    }
    differences
}

/// Every directory the database names must be present on the side its state
/// says: a live row in `assets/`, a trashed (not purged) row in `trash/`.
/// Extra directories are fine: startup recovery sweeps them.
fn verify_directories(
    tree: &Path,
    live_ids: &[String],
    trashed_ids: &[String],
) -> Result<(), ApiError> {
    let mut missing = Vec::new();
    for (location, ids) in [(ASSETS_MEMBER, live_ids), (TRASH_MEMBER, trashed_ids)] {
        for raw in ids {
            let Ok(id) = AssetId::parse(raw) else {
                return Err(invalid_backup(format!(
                    "The database names an invalid asset id: {raw}."
                )));
            };
            let directory = tree.join(location).join(id.as_str());
            match fs::metadata(&directory) {
                Ok(metadata) if metadata.is_dir() => {}
                _ => missing.push(format!("{location}/{raw}")),
            }
        }
    }
    if !missing.is_empty() {
        let shown = missing
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(invalid_backup(format!(
            "The bundle is missing {} asset director{} the database names: {shown}{}.",
            missing.len(),
            if missing.len() == 1 { "y" } else { "ies" },
            if missing.len() > 5 { ", …" } else { "" }
        )));
    }
    Ok(())
}

async fn ensure_backup_space(state: &AppState) -> Result<(), ApiError> {
    let data_dir = state.config.data_dir.clone();
    let needed = tokio::task::spawn_blocking(move || bundle_size(&data_dir))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let floor = state.config.min_free_bytes;
    let space = disk::space(&state.config.data_dir).map_err(ApiError::internal)?;
    // The staging tar is the second full copy; the snapshot is a third but
    // small next to the media. Refusing before the snapshot is what protects
    // the live data the backup exists to save.
    if space.free_bytes < needed.saturating_mul(2).saturating_add(floor) {
        return Err(not_enough_space(
            space.free_bytes,
            needed.saturating_mul(2).saturating_add(floor),
        ));
    }
    Ok(())
}

fn ensure_restore_space(state: &AppState, declared: u64) -> Result<(), ApiError> {
    let floor = state.config.min_free_bytes;
    let space = disk::space(&state.config.data_dir).map_err(ApiError::internal)?;
    // The archive is streamed once and then extracted, so the peak is about
    // twice its size. The declared length is advisory; the stream limit still
    // holds when it is missing or wrong.
    let needed = declared.saturating_mul(2).saturating_add(floor);
    if space.free_bytes < needed {
        return Err(not_enough_space(space.free_bytes, needed));
    }
    Ok(())
}

fn bundle_size(data_dir: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    for name in [ASSETS_MEMBER, TRASH_MEMBER] {
        total = total.saturating_add(directory_size(&data_dir.join(name))?);
    }
    if let Ok(metadata) = fs::metadata(data_dir.join(DATABASE_MEMBER)) {
        total = total.saturating_add(metadata.len());
    }
    Ok(total)
}

fn directory_size(path: &Path) -> io::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else if file_type.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

/// Write the bundle. Runs on a blocking thread: the walk and the tar are pure
/// file I/O and a large library would otherwise occupy an async worker.
fn write_bundle(
    data_dir: &Path,
    snapshot: &Path,
    tar_path: &Path,
    manifest: &BackupManifest,
    live_ids: &[String],
    trashed_ids: &[String],
) -> io::Result<u64> {
    let file = File::create(tar_path)?;
    let mut builder = Builder::new(file);
    builder.mode(HeaderMode::Deterministic);
    builder.follow_symlinks(false);

    append_file(&mut builder, snapshot, DATABASE_MEMBER)?;
    let mut added_assets = HashSet::new();
    let mut added_trash = HashSet::new();
    append_tree(
        &mut builder,
        &data_dir.join(ASSETS_MEMBER),
        ASSETS_MEMBER,
        Some(&mut added_assets),
    )?;
    append_tree(
        &mut builder,
        &data_dir.join(TRASH_MEMBER),
        TRASH_MEMBER,
        Some(&mut added_trash),
    )?;

    for (label, expected, added) in [
        ("live", live_ids, &added_assets),
        ("trashed", trashed_ids, &added_trash),
    ] {
        let missing: Vec<&str> = expected
            .iter()
            .map(String::as_str)
            .filter(|id| !added.contains(*id))
            .collect();
        if !missing.is_empty() {
            return Err(io::Error::other(format!(
                "the snapshot names {} {label} asset director{} that are not on disk: {}",
                missing.len(),
                if missing.len() == 1 { "y" } else { "ies" },
                missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    let manifest_json = serde_json::to_vec_pretty(manifest)
        .map_err(|error| io::Error::other(format!("cannot serialize manifest: {error}")))?;
    append_bytes(
        &mut builder,
        MANIFEST_MEMBER,
        &manifest_json,
        manifest.created_at,
    )?;

    let mut file = builder.into_inner()?;
    use std::io::Write;
    file.flush()?;
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

fn append_file(builder: &mut Builder<File>, source: &Path, name: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            source.display()
        )));
    }
    let mut file = File::open(source)?;
    let mut header = Header::new_gnu();
    header.set_metadata(&metadata);
    header.set_path(name)?;
    header.set_size(metadata.len());
    header.set_cksum();
    builder.append(&header, &mut file)?;
    Ok(())
}

fn append_bytes(
    builder: &mut Builder<File>,
    name: &str,
    bytes: &[u8],
    mtime: i64,
) -> io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(name)?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(mtime.max(0) as u64);
    header.set_cksum();
    builder.append(&header, bytes)?;
    Ok(())
}

fn append_dir(builder: &mut Builder<File>, source: &Path, name: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let mut header = Header::new_gnu();
    header.set_metadata(&metadata);
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_path(name)?;
    header.set_cksum();
    builder.append(&header, io::empty())?;
    Ok(())
}

fn append_tree(
    builder: &mut Builder<File>,
    root: &Path,
    member: &str,
    mut asset_ids: Option<&mut HashSet<String>>,
) -> io::Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(io::Error::other(format!(
                "storage entry {} is not valid UTF-8",
                entry.path().display()
            )));
        };
        let child_member = format!("{member}/{name}");
        // Only the top-level directory name is an asset id; nested files are
        // recorded under it, but must not be mistaken for ids themselves.
        if member == ASSETS_MEMBER || member == TRASH_MEMBER {
            if !file_type.is_dir() || AssetId::parse(name).is_err() {
                return Err(io::Error::other(format!(
                    "{} is not an asset directory",
                    entry.path().display()
                )));
            }
            if let Some(ids) = asset_ids.as_deref_mut() {
                ids.insert(name.to_owned());
            }
        }
        if file_type.is_dir() {
            append_dir(builder, &entry.path(), &child_member)?;
            append_tree(
                builder,
                &entry.path(),
                &child_member,
                asset_ids.as_deref_mut(),
            )?;
        } else if file_type.is_file() {
            append_file(builder, &entry.path(), &child_member)?;
        } else {
            return Err(io::Error::other(format!(
                "storage entry {} is a symlink or special file; Atlas storage must contain regular files and directories",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

/// Extract an archive with the upload rules: only the four member shapes,
/// no absolute or parent paths, no links or device nodes, and bounded member
/// count and unpacked size.
fn extract_bundle(tar_path: &Path, tree: &Path, tar_size: u64) -> io::Result<()> {
    fs::create_dir_all(tree)?;
    let file = File::open(tar_path)?;
    let mut archive = Archive::new(file);
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);

    let mut seen = HashSet::new();
    let mut members = 0_usize;
    let mut unpacked = 0_u64;
    let mut has_manifest = false;
    let mut has_database = false;

    for entry in archive.entries()? {
        let mut entry = entry?;
        members += 1;
        if members > MAX_ARCHIVE_MEMBERS {
            return Err(io::Error::other(format!(
                "the archive has more than {MAX_ARCHIVE_MEMBERS} members"
            )));
        }
        let entry_type = entry.header().entry_type();
        if !(entry_type == EntryType::Regular || entry_type == EntryType::Directory) {
            return Err(io::Error::other(format!(
                "archive member {:?} is not a regular file or directory",
                entry.path().ok()
            )));
        }
        let path = entry.path()?.into_owned();
        let relative = safe_member_path(&path)?;
        if !seen.insert(relative.clone()) {
            return Err(io::Error::other(format!(
                "archive member {} appears more than once",
                relative.display()
            )));
        }
        let name = relative.to_string_lossy().into_owned();
        if name == MANIFEST_MEMBER {
            has_manifest = true;
        } else if name == DATABASE_MEMBER {
            has_database = true;
        }

        let destination = tree.join(&relative);
        if entry_type == EntryType::Directory {
            fs::create_dir_all(&destination)?;
            continue;
        }

        let size = entry.header().size()?;
        unpacked = unpacked.saturating_add(size);
        // A tar cannot legitimately expand past its own size: it is not
        // compressed. This is the archive analogue of the pixel-count cap.
        if unpacked > tar_size {
            return Err(io::Error::other(
                "the archive's unpacked size exceeds the uploaded archive size",
            ));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&destination)?;
        let copied = io::copy(&mut entry, &mut output)?;
        if copied != size {
            return Err(io::Error::other(format!(
                "archive member {} is truncated",
                relative.display()
            )));
        }
    }

    if !has_manifest {
        return Err(io::Error::other("the archive has no manifest.json"));
    }
    if !has_database {
        return Err(io::Error::other("the archive has no atlas.sqlite3"));
    }
    // Empty collections still need their directories, so the swap and the
    // startup recovery path see a complete data directory.
    fs::create_dir_all(tree.join(ASSETS_MEMBER))?;
    fs::create_dir_all(tree.join(TRASH_MEMBER))?;
    Ok(())
}

/// Validate one member path and return it relative to the extraction root.
fn safe_member_path(path: &Path) -> io::Result<PathBuf> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(io::Error::other(format!(
            "archive member {} is absolute or empty",
            path.display()
        )));
    };
    let first = first.to_str().ok_or_else(|| {
        io::Error::other(format!("archive member {} is not UTF-8", path.display()))
    })?;
    let mut relative = PathBuf::from(first);
    let rest: Vec<&std::ffi::OsStr> = components
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => Err(io::Error::other(format!(
                "archive member {} contains a parent or root component",
                path.display()
            ))),
        })
        .collect::<Result<_, _>>()?;

    match first {
        MANIFEST_MEMBER | DATABASE_MEMBER => {
            if !rest.is_empty() {
                return Err(io::Error::other(format!(
                    "archive member {} has the wrong shape",
                    path.display()
                )));
            }
        }
        ASSETS_MEMBER | TRASH_MEMBER => {
            let Some(id) = rest.first() else {
                // The directory entry itself is allowed.
                return Ok(relative);
            };
            let id = id.to_str().ok_or_else(|| {
                io::Error::other(format!(
                    "archive member {} has a non-UTF-8 asset id",
                    path.display()
                ))
            })?;
            AssetId::parse(id).map_err(|_| {
                io::Error::other(format!(
                    "archive member {} is not under a valid asset id",
                    path.display()
                ))
            })?;
            relative.push(id);
            for value in rest.iter().skip(1) {
                relative.push(value);
            }
        }
        _ => {
            return Err(io::Error::other(format!(
                "archive member {} is outside the allowed bundle layout",
                path.display()
            )));
        }
    }
    Ok(relative)
}

fn not_enough_space(free_bytes: u64, needed: u64) -> ApiError {
    ApiError::new(
        StatusCode::INSUFFICIENT_STORAGE,
        "insufficient_storage",
        format!(
            "The data directory has {free_bytes} bytes free; this operation needs at least {needed} and keeps the configured floor in reserve."
        ),
    )
}

fn invalid_backup(detail: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_backup", detail)
}

fn backup_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "backup_not_found",
        "No backup with that id exists.",
    )
}

fn restore_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "restore_not_found",
        "No validated restore upload with that id exists; upload the bundle first.",
    )
}

fn parse_backup_id(value: &str) -> Result<String, ApiError> {
    if is_short_id(value) {
        Ok(value.to_owned())
    } else {
        Err(backup_not_found())
    }
}

fn parse_upload_id(value: &str) -> Result<String, ApiError> {
    if is_short_id(value) {
        Ok(value.to_owned())
    } else {
        Err(restore_not_found())
    }
}

/// Backup and upload ids are random 16-character lowercase hex, which cannot
/// carry a path separator or a parent component.
fn is_short_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn random_id() -> Result<String, crate::ident::IdentError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| crate::ident::IdentError::Entropy)?;
    Ok(hex::encode(bytes))
}

fn unix_seconds(time: Option<SystemTime>) -> i64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Format Unix seconds as the UTC stamp used in a download name.
fn utc_stamp(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant's `civil_from_days`,
/// with the era shifted so negative timestamps work too.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path, time::Duration};

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tar::{Builder, EntryType, Header};
    use tower::ServiceExt;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::{civil_from_days, extract_bundle, safe_member_path, utc_stamp, verify_directories};
    use crate::{AppState, Config, app, store::Store};

    #[test]
    fn utc_stamps_are_the_download_name_shape() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_767_225_600), "20260101T000000Z");
    }

    #[test]
    fn civil_dates_handle_the_epoch_and_a_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn member_paths_are_relative_and_allowlisted() {
        assert_eq!(
            safe_member_path(Path::new("assets/abcdefghijklmnop/w320.jpg")).unwrap(),
            Path::new("assets/abcdefghijklmnop/w320.jpg")
        );
        assert!(safe_member_path(Path::new("manifest.json")).is_ok());
        assert!(safe_member_path(Path::new("atlas.sqlite3")).is_ok());
        assert!(safe_member_path(Path::new("../atlas.sqlite3")).is_err());
        assert!(safe_member_path(Path::new("/etc/passwd")).is_err());
        assert!(safe_member_path(Path::new("staging/backups/x.tar")).is_err());
        assert!(safe_member_path(Path::new("assets/not-an-id/w320.jpg")).is_err());
        assert!(safe_member_path(Path::new("assets/abcdefghijklmnop/../../x")).is_err());
    }

    fn test_config(data_dir: std::path::PathBuf) -> Config {
        Config {
            listen_addr: "127.0.0.1:0".to_owned(),
            data_dir,
            api_token: "prototype-test-token".to_owned(),
            id_length: 18,
            max_image_bytes: 1024 * 1024,
            max_document_bytes: 1024,
            max_consumer_data_bytes: 1024,
            backup_upload_max_bytes: 16 * 1024 * 1024,
            min_free_bytes: 0,
            max_image_pixels: 40_000_000,
            max_image_dimension: 12_000,
            variant_widths: vec![320, 640],
            magick_bin: "magick".to_owned(),
            magick_timeout: Duration::from_secs(45),
            image_processing_timeout: Duration::from_secs(120),
            conversion_concurrency: 1,
            trash_retention_days: 30,
        }
    }

    fn request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer prototype-test-token")
            .body(Body::empty())
            .unwrap()
    }

    fn upload_request(content_type: &str, body: Vec<u8>) -> Request<Body> {
        let collection = if content_type.starts_with("image/") {
            "images"
        } else {
            "documents"
        };
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/{collection}"))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        (status, value)
    }

    async fn upload_document(router: &Router, body: &[u8]) -> String {
        let (status, value) = send(router, upload_request("application/pdf", body.to_vec())).await;
        assert_eq!(status, StatusCode::CREATED, "{value}");
        value["id"].as_str().unwrap().to_owned()
    }

    fn generated_png() -> Request<Body> {
        let output = std::process::Command::new("magick")
            .args(["-size", "12x12", "xc:red", "png:-"])
            .output()
            .unwrap();
        assert!(output.status.success(), "could not generate a test PNG");
        upload_request("image/png", output.stdout)
    }

    async fn download(router: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let (status, _, body) = download_headers(router, uri).await;
        (status, body)
    }

    async fn download_headers(
        router: &Router,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let response = router.clone().oneshot(request("GET", uri)).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    fn write_tar(path: &Path, members: &[(&str, &[u8], EntryType)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut builder = Builder::new(file);
        for (name, bytes, entry_type) in members {
            let mut header = Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_entry_type(*entry_type);
            header.set_cksum();
            builder.append(&header, *bytes).unwrap();
        }
        builder.finish().unwrap();
    }

    #[test]
    fn extraction_refuses_traversal_symlinks_and_unexpected_members() {
        let directory = tempfile::tempdir().unwrap();
        let cases: Vec<(&str, &[u8], EntryType)> = vec![
            ("assets/abcdefghijklmnop/link", b"", EntryType::Symlink),
            ("staging/backups/old.tar", b"x", EntryType::Regular),
            ("assets/not-an-id/file", b"x", EntryType::Regular),
        ];
        for (index, (name, bytes, entry_type)) in cases.into_iter().enumerate() {
            let tar = directory.path().join(format!("bad-{index}.tar"));
            write_tar(
                &tar,
                &[
                    ("manifest.json", b"{}", EntryType::Regular),
                    (name, bytes, entry_type),
                ],
            );
            let tree = directory.path().join(format!("tree-{index}"));
            let result = extract_bundle(&tar, &tree, std::fs::metadata(&tar).unwrap().len());
            assert!(result.is_err(), "accepted archive member {name}");
        }
    }

    #[test]
    fn extraction_refuses_a_parent_path_in_a_raw_header() {
        let directory = tempfile::tempdir().unwrap();
        let tar = directory.path().join("traversal.tar");
        let mut header = Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o600);
        header.set_entry_type(EntryType::Regular);
        // The tar crate refuses to *write* `..` through `set_path`, which is
        // exactly what an attacker would not use.
        header.as_mut_bytes()[..11].copy_from_slice(b"../escape\0\0");
        header.set_cksum();
        let mut file = std::fs::File::create(&tar).unwrap();
        file.write_all(header.as_bytes()).unwrap();
        file.write_all(b"x").unwrap();
        file.write_all(&[0_u8; 511]).unwrap();
        file.write_all(&[0_u8; 1024]).unwrap();
        drop(file);

        let tree = directory.path().join("tree");
        assert!(extract_bundle(&tar, &tree, std::fs::metadata(&tar).unwrap().len()).is_err());
    }

    #[test]
    fn a_truncated_archive_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let tar = directory.path().join("truncated.tar");
        write_tar(
            &tar,
            &[
                ("manifest.json", b"{}", EntryType::Regular),
                (
                    "assets/abcdefghijklmnop/file",
                    b"0123456789",
                    EntryType::Regular,
                ),
            ],
        );
        let size = std::fs::metadata(&tar).unwrap().len();
        let file = std::fs::OpenOptions::new().write(true).open(&tar).unwrap();
        file.set_len(size - 4).unwrap();
        let tree = directory.path().join("tree");
        assert!(extract_bundle(&tar, &tree, size).is_err());
    }

    #[tokio::test]
    async fn a_backup_can_be_restored_and_replaces_the_live_instance() {
        let directory = tempfile::tempdir().unwrap();
        let config = test_config(directory.path().join("atlas"));
        let state = AppState::new(config).await.unwrap();
        let router = app(state.clone());

        let kept = upload_document(&router, b"%PDF-1.4\nkept").await;
        let trashed = upload_document(&router, b"%PDF-1.4\ntrashed").await;
        let pinned = upload_document(&router, b"%PDF-1.4\npinned").await;
        let purged = upload_document(&router, b"%PDF-1.4\npurged").await;

        for id in [&trashed, &purged] {
            let (status, _) = send(
                &router,
                request("DELETE", &format!("/api/v1/documents/{id}")),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let (status, image) = send(&router, generated_png()).await;
        assert_eq!(status, StatusCode::CREATED, "{image}");
        let image = image["id"].as_str().unwrap().to_owned();
        let re_encode = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/images/{image}/renditions"))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"re_encode":true}"#))
            .unwrap();
        let (status, refreshed) = send(&router, re_encode).await;
        assert_eq!(status, StatusCode::OK, "{refreshed}");
        assert_eq!(refreshed["generation"], 2);

        let pin = Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/documents/{pinned}/pin"))
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"pinned":true}"#))
            .unwrap();
        let (status, _) = send(&router, pin).await;
        assert_eq!(status, StatusCode::OK);

        // Make the purged asset old enough for retention and sweep it, so the
        // bundle carries a tombstone with no directory.
        let purged_id = crate::ident::AssetId::parse(&purged).unwrap();
        state
            .store
            .backdate_deletion(&purged_id, crate::unix_now().unwrap() - 40 * 86_400)
            .await;
        *state.last_purge.lock().await = None;
        state.purge_expired_trash().await.unwrap();

        let (status, backup) = send(&router, request("POST", "/api/v1/backups")).await;
        assert_eq!(status, StatusCode::CREATED, "{backup}");
        let backup_id = backup["id"].as_str().unwrap().to_owned();

        let (status, list) = send(&router, request("GET", "/api/v1/backups")).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert_eq!(list["backups"][0]["id"], backup_id);
        assert!(list["backups"][0]["bytes"].as_u64().unwrap() > 0);

        let (status, archive) =
            download(&router, &format!("/api/v1/backups/{backup_id}/archive")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!archive.is_empty());

        // Change the live instance: add an asset and trash the one in the
        // snapshot. A restore must undo both.
        let _newer = upload_document(&router, b"%PDF-1.4\nnewer").await;
        let (status, _) = send(
            &router,
            request("DELETE", &format!("/api/v1/documents/{kept}")),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let restore = Request::builder()
            .method("POST")
            .uri("/api/v1/backups/restore")
            .header("authorization", "Bearer prototype-test-token")
            .header("content-type", "application/x-tar")
            .body(Body::from(archive))
            .unwrap();
        let (status, report) = send(&router, restore).await;
        assert_eq!(status, StatusCode::OK, "{report}");
        let upload_id = report["upload_id"].as_str().unwrap().to_owned();
        assert!(report["warnings"].as_array().is_some());
        assert!(report["created_at"].as_i64().is_some());

        let (status, applied) = send(
            &router,
            request(
                "POST",
                &format!("/api/v1/backups/restore/{upload_id}/apply"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{applied}");

        let (status, listing) = send(&router, request("GET", "/api/v1/documents?limit=10")).await;
        assert_eq!(status, StatusCode::OK, "{listing}");
        assert_eq!(listing["total"], 2, "{listing}");
        let active: Vec<&str> = listing["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| asset["id"].as_str().unwrap())
            .collect();
        assert!(active.contains(&kept.as_str()));
        assert!(active.contains(&pinned.as_str()));

        let (status, pinned_asset) = send(
            &router,
            request("GET", &format!("/api/v1/documents/{pinned}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pinned_asset["pinned"], true);
        assert_eq!(pinned_asset["state"], "active");

        let (status, trash) = send(&router, request("GET", "/api/v1/trash?limit=10")).await;
        assert_eq!(status, StatusCode::OK, "{trash}");
        let states: Vec<(&str, &str)> = trash["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|asset| {
                (
                    asset["id"].as_str().unwrap(),
                    asset["state"].as_str().unwrap(),
                )
            })
            .collect();
        assert!(states.contains(&(trashed.as_str(), "deleted")));
        assert!(states.contains(&(purged.as_str(), "purged")));

        let (status, _) = send(
            &router,
            request("GET", &format!("/api/v1/documents/{kept}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, restored_image) =
            send(&router, request("GET", &format!("/api/v1/images/{image}"))).await;
        assert_eq!(status, StatusCode::OK, "{restored_image}");
        assert_eq!(restored_image["generation"], 2);
        let (status, _, bytes) =
            download_headers(&router, &format!("/media/{image}/w320?g=2")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!bytes.is_empty());

        let (status, _) = send(
            &router,
            request("DELETE", &format!("/api/v1/backups/{backup_id}")),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, list) = send(&router, request("GET", "/api/v1/backups")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["backups"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_crash_mid_swap_is_rolled_back_before_the_database_opens() {
        let directory = tempfile::tempdir().unwrap();
        let config = test_config(directory.path().join("atlas"));
        let state = AppState::new(config.clone()).await.unwrap();
        let router = app(state.clone());
        let kept = upload_document(&router, b"%PDF-1.4\ncrashed").await;
        state.store.close_pool().await;

        let data_dir = config.data_dir.clone();
        let upload_id = "abcdefabcdefabcd";
        super::write_journal(&data_dir, upload_id).await.unwrap();

        // Simulate a crash after the old directories and database were moved
        // aside, but before the staged database became live.
        let rollback = data_dir
            .join("staging/restores")
            .join(upload_id)
            .join("rollback");
        std::fs::create_dir_all(&rollback).unwrap();
        std::fs::rename(data_dir.join("assets"), rollback.join("assets")).unwrap();
        std::fs::create_dir_all(data_dir.join("assets")).unwrap();
        std::fs::rename(data_dir.join("trash"), rollback.join("trash")).unwrap();
        std::fs::create_dir_all(data_dir.join("trash")).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let live = data_dir.join(format!("atlas.sqlite3{suffix}"));
            if live.exists() {
                std::fs::rename(&live, rollback.join(format!("atlas.sqlite3{suffix}"))).unwrap();
            }
        }
        assert!(!data_dir.join("atlas.sqlite3").exists());
        drop(router);
        drop(state);

        // Starting Atlas must undo the swap before opening a database that
        // `create_if_missing` would otherwise recreate empty.
        let state = AppState::new(config).await.unwrap();
        assert!(!super::journal_path(&data_dir).exists());
        let id = crate::ident::AssetId::parse(&kept).unwrap();
        assert!(state.store.get_asset(&id).await.unwrap().is_some());
        assert!(
            data_dir
                .join("assets")
                .join(&kept)
                .join("original.pdf")
                .exists()
        );
    }

    #[tokio::test]
    async fn a_newer_schema_is_reported_so_validation_can_refuse_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("newer.sqlite3");
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let newer = crate::store::latest_schema_version() + 1;
        sqlx::query(
            "INSERT INTO _sqlx_migrations
                (version, description, success, checksum, execution_time)
             VALUES (?, 'from a newer Atlas', 1, X'00', 0)",
        )
        .bind(newer)
        .execute(&pool)
        .await
        .unwrap();
        drop(pool);

        let inspection = Store::inspect_snapshot(&path).await.unwrap();
        assert_eq!(inspection.schema_version, newer);
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_database_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("junk.sqlite3");
        std::fs::write(&path, b"this is not sqlite").unwrap();
        assert!(Store::inspect_snapshot(&path).await.is_err());
    }

    #[test]
    fn a_snapshot_naming_a_missing_asset_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("tree");
        std::fs::create_dir_all(tree.join("assets/abcdefghijklmnop")).unwrap();
        let ids = vec!["abcdefghijklmnop".to_owned()];
        assert!(verify_directories(&tree, &ids, &[]).is_ok());
        std::fs::remove_dir(tree.join("assets/abcdefghijklmnop")).unwrap();
        assert!(verify_directories(&tree, &ids, &[]).is_err());
        std::fs::create_dir_all(tree.join("trash/abcdefghijklmnop")).unwrap();
        assert!(verify_directories(&tree, &[], &ids).is_ok());
        std::fs::remove_dir(tree.join("trash/abcdefghijklmnop")).unwrap();
        assert!(verify_directories(&tree, &[], &ids).is_err());
    }

    #[tokio::test]
    async fn a_backup_refuses_to_start_while_one_is_running() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(test_config(directory.path().join("atlas")))
            .await
            .unwrap();
        let router = app(state.clone());
        let _running = state.backup_lock.lock().await;
        let (status, error) = send(&router, request("POST", "/api/v1/backups")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert_eq!(error["error"]["code"], "backup_in_progress");
    }

    #[tokio::test]
    async fn a_backup_is_refused_when_the_volume_cannot_hold_another_copy() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.min_free_bytes = u64::MAX;
        let router = app(AppState::new(config).await.unwrap());
        let (status, error) = send(&router, request("POST", "/api/v1/backups")).await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{error}");
        assert_eq!(error["error"]["code"], "insufficient_storage");
    }

    #[tokio::test]
    async fn restore_upload_enforces_its_own_byte_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(directory.path().join("atlas"));
        config.backup_upload_max_bytes = 64;
        let router = app(AppState::new(config).await.unwrap());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/backups/restore")
                    .header("authorization", "Bearer prototype-test-token")
                    .header("content-type", "application/x-tar")
                    .body(Body::from(vec![0_u8; 512]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}

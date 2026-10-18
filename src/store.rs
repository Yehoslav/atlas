use std::{collections::HashMap, error::Error, fmt, path::Path};

use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::{
    ident::AssetId,
    model::{AssetRecord, AssetState, RenditionFormat, RenditionRecord, SourceFormat, StoredAsset},
};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// A row reported by `PRAGMA foreign_key_check`.
#[derive(Clone, Debug)]
pub struct ForeignKeyViolation {
    pub table: String,
    pub rowid: i64,
    pub parent: String,
    pub fkid: i64,
}

impl fmt::Display for ForeignKeyViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "row {} in {} (fk #{}) references a missing row in {}",
            self.rowid, self.table, self.fkid, self.parent
        )
    }
}

#[derive(Debug)]
pub enum StoreError {
    Database(sqlx::Error),
    Migration(sqlx::migrate::MigrateError),
    /// `PRAGMA integrity_check` did not return `ok`.
    Corrupt(String),
    /// `PRAGMA foreign_key_check` reported orphan rows.
    ForeignKeys(Vec<ForeignKeyViolation>),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "database error: {error}"),
            Self::Migration(error) => write!(formatter, "schema migration failed: {error}"),
            Self::Corrupt(report) => {
                write!(formatter, "SQLite integrity check failed: {report}")
            }
            Self::ForeignKeys(violations) => {
                write!(formatter, "SQLite foreign key check failed:")?;
                for violation in violations {
                    write!(formatter, " {violation};")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for StoreError {}

impl From<sqlx::Error> for StoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(value: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(value)
    }
}

/// Outcome of an insert, distinguishing a lost ID race from a real failure so
/// the caller can retry with a freshly generated ID.
pub enum InsertOutcome {
    Created,
    DuplicateId,
}

impl Store {
    pub async fn connect(path: &Path) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;
        let store = Self { pool };
        store.check_integrity().await?;
        Ok(store)
    }

    /// Refuse to serve from a database that is corrupt or has orphaned rows.
    ///
    /// Both checks run after migrations, so a failed migration is reported as a
    /// migration error and a database that is merely inconsistent is reported
    /// here rather than silently read.
    pub async fn check_integrity(&self) -> Result<(), StoreError> {
        let rows = sqlx::query("PRAGMA integrity_check")
            .fetch_all(&self.pool)
            .await?;
        let report: Vec<String> = rows
            .iter()
            .filter_map(|row| row.try_get::<String, _>(0).ok())
            .collect();
        if report.len() != 1 || &report[0] != "ok" {
            let report = if report.is_empty() {
                "no result".to_owned()
            } else {
                report.join("; ")
            };
            tracing::error!("Atlas metadata database is corrupt: {report}");
            return Err(StoreError::Corrupt(report));
        }

        let violations = self.foreign_key_violations().await?;
        if !violations.is_empty() {
            for violation in &violations {
                tracing::error!("Atlas metadata database foreign key violation: {violation}");
            }
            return Err(StoreError::ForeignKeys(violations));
        }

        Ok(())
    }

    pub async fn foreign_key_violations(&self) -> Result<Vec<ForeignKeyViolation>, StoreError> {
        let rows = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(ForeignKeyViolation {
                    table: row.try_get("table").ok()?,
                    rowid: row.try_get("rowid").ok()?,
                    parent: row.try_get("parent").ok()?,
                    fkid: row.try_get("fkid").ok()?,
                })
            })
            .collect())
    }

    pub async fn insert_asset(
        &self,
        asset: &AssetRecord,
        renditions: &[RenditionRecord],
    ) -> Result<InsertOutcome, StoreError> {
        let mut transaction = self.pool.begin().await?;
        let insert = sqlx::query(
            "INSERT INTO assets (id, source_format, bytes, sha256, width, height, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(asset.id.as_str())
        .bind(asset.source_format.key())
        .bind(i64::try_from(asset.bytes).unwrap_or(i64::MAX))
        .bind(&asset.sha256)
        .bind(asset.width.map(i64::from))
        .bind(asset.height.map(i64::from))
        .bind(asset.created_at)
        .execute(&mut *transaction)
        .await;

        if let Err(error) = insert {
            return if Self::is_unique_violation(&error) {
                Ok(InsertOutcome::DuplicateId)
            } else {
                Err(StoreError::Database(error))
            };
        }

        for rendition in renditions {
            sqlx::query(
                "INSERT INTO renditions
                    (asset_id, max_width, width, height, format, bytes, sha256, file_name)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(asset.id.as_str())
            .bind(i64::from(rendition.max_width))
            .bind(i64::from(rendition.width))
            .bind(i64::from(rendition.height))
            .bind(rendition.format.key())
            .bind(i64::try_from(rendition.bytes).unwrap_or(i64::MAX))
            .bind(&rendition.sha256)
            .bind(&rendition.file_name)
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;
        Ok(InsertOutcome::Created)
    }

    /// SQLite extended result codes for a primary-key or unique constraint failure.
    fn is_unique_violation(error: &sqlx::Error) -> bool {
        match error {
            sqlx::Error::Database(database) => database
                .code()
                .is_some_and(|code| matches!(code.as_ref(), "1555" | "2067")),
            _ => false,
        }
    }

    /// Every asset ID with its lifecycle state, for reconciling stored files
    /// against the index at startup.
    pub async fn asset_states(&self) -> Result<HashMap<String, AssetState>, StoreError> {
        let rows = sqlx::query("SELECT id, deleted_at, purged_at FROM assets")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let id: String = row.try_get("id").ok()?;
                Some((id, state_from(&row)))
            })
            .collect())
    }

    pub async fn get_asset(&self, id: &AssetId) -> Result<Option<StoredAsset>, StoreError> {
        let row = sqlx::query(
            "SELECT id, source_format, bytes, sha256, width, height, created_at,
                    deleted_at, purged_at, pinned
             FROM assets WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let source_format = row
            .try_get::<String, _>("source_format")
            .ok()
            .and_then(|value| SourceFormat::from_database(&value));
        let Some(source_format) = source_format else {
            return Ok(None);
        };

        let asset = AssetRecord {
            id: id.clone(),
            source_format,
            bytes: row.try_get::<i64, _>("bytes")?.max(0) as u64,
            sha256: row.try_get("sha256")?,
            width: row
                .try_get::<Option<i64>, _>("width")?
                .and_then(|value| u32::try_from(value).ok()),
            height: row
                .try_get::<Option<i64>, _>("height")?
                .and_then(|value| u32::try_from(value).ok()),
            created_at: row.try_get("created_at")?,
            deleted_at: row.try_get("deleted_at")?,
            purged_at: row.try_get("purged_at")?,
            pinned: row.try_get::<bool, _>("pinned")?,
        };

        let rows = sqlx::query(
            "SELECT max_width, width, height, format, bytes, sha256, file_name
             FROM renditions WHERE asset_id = ? ORDER BY max_width ASC, format ASC",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;

        let renditions = rows.into_iter().filter_map(to_rendition).collect();

        Ok(Some(StoredAsset { asset, renditions }))
    }

    /// Stamp the soft delete. Callers must move the files out of `assets/` only
    /// after this succeeds, so that a crash leaves bytes Atlas still refuses to
    /// publish rather than a live row whose bytes have vanished.
    pub async fn mark_deleted(&self, id: &AssetId, now: i64) -> Result<bool, StoreError> {
        let result =
            sqlx::query("UPDATE assets SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_restored(&self, id: &AssetId) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE assets SET deleted_at = NULL, purged_at = NULL
             WHERE id = ? AND deleted_at IS NOT NULL",
        )
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_pinned(&self, id: &AssetId, pinned: bool) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE assets SET pinned = ? WHERE id = ?")
            .bind(pinned)
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Assets past the retention window that no consumer has pinned. Already
    /// purged tombstones are not candidates again.
    pub async fn purge_candidates(&self, cutoff: i64) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT id FROM assets
             WHERE deleted_at IS NOT NULL AND purged_at IS NULL AND pinned = 0
               AND deleted_at <= ?
             ORDER BY deleted_at ASC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("id").ok())
            .collect())
    }

    /// Keep the row as a tombstone and drop the metadata that described bytes
    /// which are about to disappear. The rendition rows go with the files: a
    /// tombstone that still advertised rendition URLs would report URLs whose
    /// bytes no longer exist.
    pub async fn mark_purged(&self, id: &AssetId, now: i64) -> Result<bool, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM renditions WHERE asset_id = ?")
            .bind(id.as_str())
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("UPDATE assets SET purged_at = ? WHERE id = ?")
            .bind(now)
            .bind(id.as_str())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Soft-deleted and purged assets, newest deletion first.
    pub async fn trashed_assets(&self) -> Result<Vec<StoredAsset>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, source_format, bytes, sha256, width, height, created_at,
                    deleted_at, purged_at, pinned
             FROM assets WHERE deleted_at IS NOT NULL
             ORDER BY deleted_at DESC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut assets = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.try_get::<String, _>("id")?;
            let Some(id) = AssetId::parse(&id).ok() else {
                tracing::error!("stored asset id is not a valid identifier: {id}");
                continue;
            };
            let Some(source_format) = row
                .try_get::<String, _>("source_format")
                .ok()
                .and_then(|value| SourceFormat::from_database(&value))
            else {
                tracing::error!("asset {id} has an unknown source format and is skipped");
                continue;
            };

            assets.push(StoredAsset {
                asset: AssetRecord {
                    id,
                    source_format,
                    bytes: row.try_get::<i64, _>("bytes")?.max(0) as u64,
                    sha256: row.try_get("sha256")?,
                    width: row
                        .try_get::<Option<i64>, _>("width")?
                        .and_then(|value| u32::try_from(value).ok()),
                    height: row
                        .try_get::<Option<i64>, _>("height")?
                        .and_then(|value| u32::try_from(value).ok()),
                    created_at: row.try_get("created_at")?,
                    deleted_at: row.try_get("deleted_at")?,
                    purged_at: row.try_get("purged_at")?,
                    pinned: row.try_get::<bool, _>("pinned")?,
                },
                renditions: Vec::new(),
            });
        }

        Ok(assets)
    }

    /// Move a deletion into the past so retention can be exercised without waiting.
    #[cfg(test)]
    pub(crate) async fn backdate_deletion(&self, id: &AssetId, deleted_at: i64) {
        sqlx::query("UPDATE assets SET deleted_at = ? WHERE id = ?")
            .bind(deleted_at)
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .expect("the asset row exists");
    }
}

fn to_rendition(row: sqlx::sqlite::SqliteRow) -> Option<RenditionRecord> {
    let format = row
        .try_get::<String, _>("format")
        .ok()
        .and_then(|value| RenditionFormat::from_database(&value))?;
    let max_width = u32::try_from(row.try_get::<i64, _>("max_width").ok()?).ok()?;
    let width = u32::try_from(row.try_get::<i64, _>("width").ok()?).ok()?;
    let file_name = match row.try_get::<Option<String>, _>("file_name").ok().flatten() {
        Some(name) => name,
        None => {
            // A row written before renditions recorded their file name. The
            // backfill in migration 0003 covers the database as a whole; this
            // keeps one odd row from silently losing its rendition. Files have
            // always been named after the actual width.
            tracing::warn!(asset = %row.try_get::<String, _>("asset_id").unwrap_or_default(), max_width, "rendition row has no file name; using the legacy name");
            format!("w{width}.{}", format.extension())
        }
    };

    Some(RenditionRecord {
        max_width,
        width,
        height: u32::try_from(row.try_get::<i64, _>("height").ok()?).ok()?,
        format,
        bytes: row.try_get::<i64, _>("bytes").ok()?.max(0) as u64,
        sha256: row.try_get("sha256").ok()?,
        file_name,
    })
}

fn state_from(row: &sqlx::sqlite::SqliteRow) -> AssetState {
    if row
        .try_get::<Option<i64>, _>("purged_at")
        .ok()
        .flatten()
        .is_some()
    {
        AssetState::Purged
    } else if row
        .try_get::<Option<i64>, _>("deleted_at")
        .ok()
        .flatten()
        .is_some()
    {
        AssetState::Deleted
    } else {
        AssetState::Active
    }
}

#[cfg(test)]
mod tests {
    use sqlx::Acquire;

    use super::{InsertOutcome, Store, StoreError};
    use crate::{
        MIGRATION_FILES,
        ident::{AssetId, DEFAULT_LENGTH},
        model::{AssetRecord, AssetState, RenditionFormat, RenditionRecord, SourceFormat},
    };

    fn record(id: &AssetId) -> AssetRecord {
        AssetRecord {
            id: id.clone(),
            source_format: SourceFormat::Jpeg,
            bytes: 1024,
            sha256: "abc123".to_owned(),
            width: Some(640),
            height: Some(480),
            created_at: 1_700_000_000,
            deleted_at: None,
            purged_at: None,
            pinned: false,
        }
    }

    fn rendition() -> RenditionRecord {
        RenditionRecord {
            max_width: 320,
            width: 320,
            height: 240,
            format: RenditionFormat::Jpeg,
            bytes: 512,
            sha256: "def456".to_owned(),
            file_name: "w320.jpg".to_owned(),
        }
    }

    async fn store_with_asset(directory: &std::path::Path) -> (Store, AssetId) {
        let store = Store::connect(&directory.join("atlas.sqlite3"))
            .await
            .unwrap();
        let id = AssetId::generate(DEFAULT_LENGTH).unwrap();
        let outcome = store
            .insert_asset(&record(&id), &[rendition()])
            .await
            .unwrap();
        assert!(matches!(outcome, InsertOutcome::Created));
        (store, id)
    }

    #[test]
    fn every_migration_file_is_embedded_and_ordered() {
        assert!(!MIGRATION_FILES.is_empty());
        assert!(
            MIGRATION_FILES
                .iter()
                .all(|name| name.ends_with(".sql") && name.len() > 5),
            "{MIGRATION_FILES:?}"
        );
        let mut sorted = MIGRATION_FILES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, MIGRATION_FILES, "migrations are applied in order");
    }

    #[tokio::test]
    async fn fresh_database_reports_no_integrity_problems() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&directory.path().join("atlas.sqlite3"))
            .await
            .unwrap();
        assert!(store.foreign_key_violations().await.unwrap().is_empty());
        store.check_integrity().await.unwrap();
    }

    #[tokio::test]
    async fn orphaned_rows_are_reported_and_block_startup() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&directory.path().join("atlas.sqlite3"))
            .await
            .unwrap();

        // Written with enforcement off: only a migration or a bug can do this.
        let mut connection = store.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO renditions
                (asset_id, max_width, width, height, format, bytes, sha256)
             VALUES ('aaaaaaaaaaaaaaaaaa', 320, 320, 240, 'jpeg', 1, 'x')",
        )
        .execute(&mut *connection)
        .await
        .unwrap();

        let violations = store.foreign_key_violations().await.unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].table, "renditions");
        assert!(matches!(
            store.check_integrity().await,
            Err(StoreError::ForeignKeys(_))
        ));
    }

    /// The 12-step rebuild from the SQLite documentation, and the procedure
    /// `migrations/README.md` documents for Atlas.
    ///
    /// A new table is created under a temporary name and swapped in, because
    /// `ALTER TABLE ... ADD COLUMN` cannot change a column's type or drop a
    /// constraint, and `DROP TABLE assets` would fire the `renditions`
    /// `ON DELETE CASCADE` if foreign keys were still enforced.
    async fn rebuild_assets_with_note_column(store: &Store, guard: bool) {
        let mut connection = store.pool.acquire().await.unwrap();
        if guard {
            // Outside the transaction: SQLite ignores the pragma inside one,
            // and the pragma applies to this connection only.
            sqlx::query("PRAGMA foreign_keys = OFF")
                .execute(&mut *connection)
                .await
                .unwrap();
        }

        let mut transaction = connection.begin().await.unwrap();
        sqlx::query(
            "CREATE TABLE assets_new (
                id TEXT PRIMARY KEY NOT NULL,
                source_format TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                width INTEGER,
                height INTEGER,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER,
                purged_at INTEGER,
                pinned INTEGER NOT NULL DEFAULT 0,
                note TEXT
            )",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO assets_new (id, source_format, bytes, sha256, width, height,
                                     created_at, deleted_at, purged_at, pinned)
             SELECT id, source_format, bytes, sha256, width, height,
                    created_at, deleted_at, purged_at, pinned
             FROM assets",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query("DROP TABLE assets")
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE assets_new RENAME TO assets")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        if guard {
            sqlx::query("PRAGMA foreign_keys = ON")
                .execute(&mut *connection)
                .await
                .unwrap();
            sqlx::query("PRAGMA foreign_key_check")
                .execute(&mut *connection)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn guarded_table_rebuild_keeps_child_rows() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;
        rebuild_assets_with_note_column(&store, true).await;

        assert!(store.foreign_key_violations().await.unwrap().is_empty());
        let stored = store.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.renditions.len(),
            1,
            "renditions survived the rebuild"
        );
        assert_eq!(stored.asset.sha256, "abc123");
    }

    #[tokio::test]
    async fn unguarded_table_rebuild_cascades_child_rows_away() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;

        // The same rebuild with enforcement left on: `DROP TABLE` fires
        // `ON DELETE CASCADE`, the parent comes back, and the database looks
        // self-consistent while the renditions are gone.
        rebuild_assets_with_note_column(&store, false).await;

        let violations = store.foreign_key_violations().await.unwrap();
        assert!(
            violations.is_empty(),
            "the cascade leaves nothing for the foreign key check to detect: {violations:?}"
        );
        assert!(
            store
                .get_asset(&id)
                .await
                .unwrap()
                .unwrap()
                .renditions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn lifecycle_transitions_are_recorded() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;

        assert_eq!(
            store.get_asset(&id).await.unwrap().unwrap().asset.state(),
            AssetState::Active
        );

        assert!(store.mark_deleted(&id, 1_700_000_100).await.unwrap());
        assert!(
            !store.mark_deleted(&id, 1_700_000_200).await.unwrap(),
            "deleting twice is a no-op"
        );
        assert_eq!(
            store.get_asset(&id).await.unwrap().unwrap().asset.state(),
            AssetState::Deleted
        );

        assert!(store.set_pinned(&id, true).await.unwrap());
        assert!(store.get_asset(&id).await.unwrap().unwrap().asset.pinned);
        assert!(
            store
                .purge_candidates(1_700_000_500)
                .await
                .unwrap()
                .is_empty(),
            "a pinned asset is never a purge candidate"
        );

        assert!(store.mark_restored(&id).await.unwrap());
        assert_eq!(
            store.get_asset(&id).await.unwrap().unwrap().asset.state(),
            AssetState::Active
        );
        assert!(store.get_asset(&id).await.unwrap().unwrap().asset.pinned);
    }

    #[tokio::test]
    async fn purge_keeps_a_tombstone_without_rendition_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;

        store.mark_deleted(&id, 1_700_000_100).await.unwrap();
        assert_eq!(
            store.purge_candidates(1_700_000_500).await.unwrap(),
            [id.to_string()]
        );
        assert!(
            store
                .purge_candidates(1_700_000_050)
                .await
                .unwrap()
                .is_empty(),
            "assets inside the retention window are kept"
        );

        assert!(store.mark_purged(&id, 1_700_000_600).await.unwrap());
        let stored = store.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(stored.asset.state(), AssetState::Purged);
        assert!(stored.renditions.is_empty(), "purge drops rendition rows");
        assert_eq!(stored.asset.deleted_at, Some(1_700_000_100));
        assert!(store.foreign_key_violations().await.unwrap().is_empty());
        assert!(store.purge_candidates(i64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn renditions_record_the_name_of_the_file_they_were_written_to() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;
        let stored = store.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(stored.renditions.len(), 1);
        assert_eq!(stored.renditions[0].file_name, "w320.jpg");
    }

    #[tokio::test]
    async fn a_rendition_without_a_file_name_still_resolves() {
        let directory = tempfile::tempdir().unwrap();
        let (store, id) = store_with_asset(directory.path()).await;
        sqlx::query("UPDATE renditions SET file_name = NULL WHERE asset_id = ?")
            .bind(id.as_str())
            .execute(&store.pool)
            .await
            .unwrap();

        let stored = store.get_asset(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.renditions[0].file_name, "w320.jpg",
            "a row the backfill missed keeps the legacy name instead of vanishing"
        );
    }

    #[tokio::test]
    async fn trash_listing_reports_deleted_and_purged_assets() {
        let directory = tempfile::tempdir().unwrap();
        let (store, deleted) = store_with_asset(directory.path()).await;
        let purged = AssetId::generate(DEFAULT_LENGTH).unwrap();
        store.insert_asset(&record(&purged), &[]).await.unwrap();
        store.mark_deleted(&deleted, 1_700_000_100).await.unwrap();
        store.mark_deleted(&purged, 1_700_000_200).await.unwrap();
        store.mark_purged(&purged, 1_700_000_300).await.unwrap();

        let trashed = store.trashed_assets().await.unwrap();
        assert_eq!(trashed.len(), 2);
        assert_eq!(trashed[0].asset.id, purged, "newest deletion first");
        assert_eq!(trashed[0].asset.state(), AssetState::Purged);
        assert_eq!(trashed[1].asset.id, deleted);
    }
}

use std::path::Path;

use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::{
    ident::AssetId,
    model::{AssetRecord, RenditionFormat, RenditionRecord, SourceFormat, StoredAsset},
};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Outcome of an insert, distinguishing a lost ID race from a real failure so
/// the caller can retry with a freshly generated ID.
pub enum InsertOutcome {
    Created,
    DuplicateId,
}

impl Store {
    pub async fn connect(path: &Path) -> Result<Self, sqlx::Error> {
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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS assets (
                id TEXT PRIMARY KEY NOT NULL,
                source_format TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                width INTEGER,
                height INTEGER,
                created_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS renditions (
                asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
                max_width INTEGER NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                format TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                PRIMARY KEY (asset_id, max_width, format)
            )",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn insert_asset(
        &self,
        asset: &AssetRecord,
        renditions: &[RenditionRecord],
    ) -> Result<InsertOutcome, sqlx::Error> {
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
                Err(error)
            };
        }

        for rendition in renditions {
            sqlx::query(
                "INSERT INTO renditions
                    (asset_id, max_width, width, height, format, bytes, sha256)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(asset.id.as_str())
            .bind(i64::from(rendition.max_width))
            .bind(i64::from(rendition.width))
            .bind(i64::from(rendition.height))
            .bind(rendition.format.key())
            .bind(i64::try_from(rendition.bytes).unwrap_or(i64::MAX))
            .bind(&rendition.sha256)
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

    pub async fn asset_ids(&self) -> Result<std::collections::HashSet<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT id FROM assets")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("id").ok())
            .collect())
    }

    pub async fn get_asset(&self, id: &AssetId) -> Result<Option<StoredAsset>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, source_format, bytes, sha256, width, height, created_at
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
        };

        let rows = sqlx::query(
            "SELECT max_width, width, height, format, bytes, sha256
             FROM renditions WHERE asset_id = ? ORDER BY max_width ASC, format ASC",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;

        let renditions = rows
            .into_iter()
            .filter_map(|row| {
                let format = row
                    .try_get::<String, _>("format")
                    .ok()
                    .and_then(|value| RenditionFormat::from_database(&value))?;
                Some(RenditionRecord {
                    max_width: u32::try_from(row.try_get::<i64, _>("max_width").ok()?).ok()?,
                    width: u32::try_from(row.try_get::<i64, _>("width").ok()?).ok()?,
                    height: u32::try_from(row.try_get::<i64, _>("height").ok()?).ok()?,
                    format,
                    bytes: row.try_get::<i64, _>("bytes").ok()?.max(0) as u64,
                    sha256: row.try_get("sha256").ok()?,
                })
            })
            .collect();

        Ok(Some(StoredAsset { asset, renditions }))
    }
}

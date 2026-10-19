use std::{collections::HashMap, error::Error, fmt, path::Path};

use sqlx::{
    QueryBuilder, Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::{
    ident::AssetId,
    model::{
        AssetRecord, AssetState, Kind, RenditionFormat, RenditionRecord, SourceFormat, StoredAsset,
    },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sort {
    CreatedAt,
    Bytes,
    Width,
    Filename,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Order {
    Asc,
    Desc,
}

/// Which part of an asset's life a listing covers.
///
/// "Deleted" and "purged" are deliberately different questions. A deleted asset
/// still has its bytes in the trash and can be restored; a purged one is a
/// tombstone, and listing one under the other is how a caller ends up offering
/// a reader a restore that can no longer work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateFilter {
    Live,
    Deleted,
    Purged,
    All,
}

/// A browsable page of one collection.
///
/// Every field is optional except the collection and the window: an absent
/// filter means "no opinion", which is what makes a query builder's job easy.
#[derive(Clone, Debug)]
pub struct ListQuery {
    pub kind: Kind,
    pub state: StateFilter,
    /// Stored format keys this listing is limited to, already resolved from
    /// the caller's media type or group.
    pub mime: Option<Vec<&'static str>>,
    pub name_contains: Option<String>,
    pub created_after: Option<i64>,
    pub created_before: Option<i64>,
    pub bytes_min: Option<i64>,
    pub bytes_max: Option<i64>,
    pub width_min: Option<i64>,
    pub height_min: Option<i64>,
    pub sort: Sort,
    pub order: Order,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            kind: Kind::Image,
            state: StateFilter::Live,
            mime: None,
            name_contains: None,
            created_after: None,
            created_before: None,
            bytes_min: None,
            bytes_max: None,
            width_min: None,
            height_min: None,
            sort: Sort::CreatedAt,
            order: Order::Desc,
            limit: DEFAULT_LIST_LIMIT,
            offset: 0,
        }
    }
}

pub const DEFAULT_LIST_LIMIT: i64 = 50;
pub const MAX_LIST_LIMIT: i64 = 200;

/// Append the `WHERE` clause for a listing, including the leading `WHERE`.
///
/// Used by both the count and the page query so the two can never disagree
/// about what "the filtered set" means.
fn push_filters<'a>(builder: &mut QueryBuilder<'a, Sqlite>, query: &ListQuery) {
    builder.push(" WHERE kind = ");
    builder.push_bind(query.kind.key());

    match query.state {
        StateFilter::Live => {
            builder.push(" AND deleted_at IS NULL");
        }
        StateFilter::Deleted => {
            builder.push(" AND deleted_at IS NOT NULL AND purged_at IS NULL");
        }
        StateFilter::Purged => {
            builder.push(" AND purged_at IS NOT NULL");
        }
        StateFilter::All => {}
    }

    if let Some(formats) = &query.mime {
        // Resolved to keys by the caller, so this stays an equality test: no
        // LIKE, no interpolated values, and a wildcard group cannot become a
        // pattern by accident.
        builder.push(" AND source_format IN (");
        for (index, format) in formats.iter().enumerate() {
            if index > 0 {
                builder.push(", ");
            }
            builder.push_bind(*format);
        }
        builder.push(")");
    }

    if let Some(needle) = query.name_contains.as_deref() {
        builder.push(" AND original_name LIKE ");
        builder.push_bind(format!("%{}%", escape_like(needle)));
        builder.push(" ESCAPE '\\'");
    }
    for (column, value) in [
        ("created_at >=", query.created_after),
        ("created_at <=", query.created_before),
        ("bytes >=", query.bytes_min),
        ("bytes <=", query.bytes_max),
        ("width >=", query.width_min),
        ("height >=", query.height_min),
    ] {
        if let Some(value) = value {
            builder.push(format!(" AND {column} "));
            builder.push_bind(value);
        }
    }
}

/// Escape the characters `LIKE` treats as wildcards, so a search for `50%` or
/// `a_b` looks for those characters instead of matching everything.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
            "INSERT INTO assets (id, source_format, bytes, sha256, width, height, created_at,
                                 original_name, kind)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(asset.id.as_str())
        .bind(asset.source_format.key())
        .bind(i64::try_from(asset.bytes).unwrap_or(i64::MAX))
        .bind(&asset.sha256)
        .bind(asset.width.map(i64::from))
        .bind(asset.height.map(i64::from))
        .bind(asset.created_at)
        .bind(&asset.original_name)
        .bind(asset.kind.map(Kind::key))
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

    /// A live asset whose original bytes hash to `sha256` *in the same
    /// collection*.
    ///
    /// Only live assets match: re-uploading after a delete is a decision to add
    /// the file again, not to resurrect an old one. The kind is part of the
    /// lookup because identical bytes mean different things in different
    /// collections - a document must never answer a request for an image that
    /// needs renditions. This is best effort by design: see
    /// `migrations/README.md` for why a unique index is not an option.
    pub async fn find_live_by_sha256(
        &self,
        sha256: &str,
        kind: Kind,
    ) -> Result<Option<AssetId>, StoreError> {
        let row = sqlx::query(
            "SELECT id FROM assets
             WHERE sha256 = ? AND kind = ? AND deleted_at IS NULL
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(sha256)
        .bind(kind.key())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .and_then(|row| row.try_get::<String, _>("id").ok())
            .and_then(|id| AssetId::parse(&id).ok()))
    }

    /// A page of one collection, plus the size of the whole filtered set.
    pub async fn list_assets(
        &self,
        query: ListQuery,
    ) -> Result<(Vec<StoredAsset>, i64), StoreError> {
        let total: i64 = {
            let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM assets");
            push_filters(&mut count, &query);
            count.build().fetch_one(&self.pool).await?.try_get(0)?
        };

        let direction = match query.order {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        // `id` is the tiebreaker on every sort, so equal keys cannot make a row
        // appear on two pages or on none.
        let ordering = match query.sort {
            Sort::CreatedAt => format!("created_at {direction}, id {direction}"),
            Sort::Bytes => format!("bytes {direction}, id {direction}"),
            Sort::Width => format!("width {direction}, id {direction}"),
            Sort::Filename => {
                // Unnamed assets last, whichever way the list is ordered.
                format!("(original_name IS NULL) ASC, original_name {direction}, id {direction}")
            }
        };

        let mut page = QueryBuilder::<Sqlite>::new(
            "SELECT id, source_format, bytes, sha256, width, height, created_at, \
             original_name, kind, deleted_at, purged_at, pinned FROM assets",
        );
        push_filters(&mut page, &query);
        page.push(format!(" ORDER BY {ordering} LIMIT "));
        page.push_bind(query.limit);
        page.push(" OFFSET ");
        page.push_bind(query.offset);
        let rows = page.build().fetch_all(&self.pool).await?;

        let mut assets = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_id = row.try_get::<String, _>("id")?;
            let Ok(id) = AssetId::parse(&raw_id) else {
                tracing::error!("stored asset id is not a valid identifier: {raw_id}");
                continue;
            };
            let Some(asset) = asset_from_row(&row, id)? else {
                continue;
            };
            assets.push(StoredAsset {
                asset,
                renditions: Vec::new(),
            });
        }

        Ok((assets, total))
    }

    /// Counts and bytes for each collection, for a library header.
    pub async fn summary(&self) -> Result<Vec<(Kind, i64, i64)>, StoreError> {
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS count, COALESCE(SUM(bytes), 0) AS bytes
             FROM assets GROUP BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let kind = row
                    .try_get::<Option<String>, _>("kind")
                    .ok()
                    .flatten()
                    .and_then(|value| Kind::from_key(&value))?;
                Some((
                    kind,
                    row.try_get::<i64, _>("count").ok()?,
                    row.try_get::<i64, _>("bytes").ok()?.max(0),
                ))
            })
            .collect())
    }

    /// Renditions for a set of assets, in one query, for a listing page.
    pub async fn renditions_for(
        &self,
        ids: &[&AssetId],
    ) -> Result<HashMap<String, Vec<RenditionRecord>>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT asset_id, max_width, width, height, format, bytes, sha256, file_name
             FROM renditions WHERE asset_id IN (",
        );
        let mut separated = query.separated(", ");
        for id in ids {
            separated.push_bind(id.as_str());
        }
        separated.push_unseparated(") ORDER BY max_width ASC, format ASC");

        let rows = query.build().fetch_all(&self.pool).await?;
        let mut by_asset: HashMap<String, Vec<RenditionRecord>> = HashMap::new();
        for row in rows {
            if let Some((asset_id, rendition)) = to_rendition(row) {
                by_asset.entry(asset_id).or_default().push(rendition);
            }
        }
        Ok(by_asset)
    }

    /// Lifecycle state for a set of ids the caller already knows.
    ///
    /// Ids with no row are simply absent from the map: "Atlas has never heard of
    /// this" is an answer, not an error.
    pub async fn asset_states_by_id(
        &self,
        ids: &[AssetId],
    ) -> Result<HashMap<String, (AssetState, bool, Option<Kind>)>, StoreError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id, kind, deleted_at, purged_at, pinned FROM assets WHERE id IN (",
        );
        let mut separated = query.separated(", ");
        for id in ids {
            separated.push_bind(id.as_str());
        }
        separated.push_unseparated(")");

        let rows = query.build().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let id: String = row.try_get("id").ok()?;
                let pinned = row.try_get::<bool, _>("pinned").ok()?;
                let kind = row
                    .try_get::<Option<String>, _>("kind")
                    .ok()
                    .flatten()
                    .and_then(|value| Kind::from_key(&value));
                Some((id, (state_from(&row), pinned, kind)))
            })
            .collect())
    }

    pub async fn get_asset(&self, id: &AssetId) -> Result<Option<StoredAsset>, StoreError> {
        let row = sqlx::query(
            "SELECT id, source_format, bytes, sha256, width, height, created_at,
                    original_name, kind, deleted_at, purged_at, pinned
             FROM assets WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let Some(asset) = asset_from_row(&row, id.clone())? else {
            return Ok(None);
        };

        let rows = sqlx::query(
            "SELECT asset_id, max_width, width, height, format, bytes, sha256, file_name
             FROM renditions WHERE asset_id = ? ORDER BY max_width ASC, format ASC",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;

        let renditions = rows
            .into_iter()
            .filter_map(to_rendition)
            .map(|(_, rendition)| rendition)
            .collect();

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

    /// How many assets are in the trash, for paging.
    pub async fn trashed_count(&self) -> Result<i64, StoreError> {
        Ok(
            sqlx::query("SELECT COUNT(*) FROM assets WHERE deleted_at IS NOT NULL")
                .fetch_one(&self.pool)
                .await?
                .try_get::<i64, _>(0)?
                .max(0),
        )
    }

    /// Soft-deleted and purged assets, newest deletion first.
    ///
    /// A window rather than the whole trash: an admin who has deleted a few
    /// thousand assets over the years should still get a page that loads.
    pub async fn trashed_assets(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<StoredAsset>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, source_format, bytes, sha256, width, height, created_at,
                    original_name, kind, deleted_at, purged_at, pinned
             FROM assets WHERE deleted_at IS NOT NULL
             ORDER BY deleted_at DESC, id ASC
             LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let mut assets = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_id = row.try_get::<String, _>("id")?;
            let Ok(id) = AssetId::parse(&raw_id) else {
                tracing::error!("stored asset id is not a valid identifier: {raw_id}");
                continue;
            };
            let Some(asset) = asset_from_row(&row, id)? else {
                continue;
            };
            assets.push(StoredAsset {
                asset,
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

/// Build an `AssetRecord` from a row that selected the asset columns.
///
/// `None` means the row cannot be trusted: an unknown format or an identifier
/// that is not an asset id. Kind is read from the column and, for a row written
/// before that column existed, derived from the format the row already had.
fn asset_from_row(
    row: &sqlx::sqlite::SqliteRow,
    id: AssetId,
) -> Result<Option<AssetRecord>, StoreError> {
    let Some(source_format) = row
        .try_get::<String, _>("source_format")
        .ok()
        .and_then(|value| SourceFormat::from_database(&value))
    else {
        tracing::error!(asset = %id, "stored asset has an unknown source format");
        return Ok(None);
    };

    let kind = match row.try_get::<Option<String>, _>("kind").ok().flatten() {
        Some(value) => match Kind::from_key(&value) {
            Some(kind) => Some(kind),
            None => {
                tracing::error!(asset = %id, kind = %value, "stored asset has an unknown kind");
                None
            }
        },
        None => {
            tracing::warn!(asset = %id, "stored asset has no kind; deriving it from its format");
            Some(source_format.kind())
        }
    };

    Ok(Some(AssetRecord {
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
        original_name: row.try_get("original_name")?,
        kind,
        deleted_at: row.try_get("deleted_at")?,
        purged_at: row.try_get("purged_at")?,
        pinned: row.try_get::<bool, _>("pinned")?,
    }))
}

/// The asset ID and the rendition it describes, or `None` for a row Atlas cannot
/// make sense of.
fn to_rendition(row: sqlx::sqlite::SqliteRow) -> Option<(String, RenditionRecord)> {
    let asset_id: String = row.try_get("asset_id").ok()?;
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

    Some((
        asset_id,
        RenditionRecord {
            max_width,
            width,
            height: u32::try_from(row.try_get::<i64, _>("height").ok()?).ok()?,
            format,
            bytes: row.try_get::<i64, _>("bytes").ok()?.max(0) as u64,
            sha256: row.try_get("sha256").ok()?,
            file_name,
        },
    ))
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
        model::Kind,
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
            original_name: None,
            kind: Some(Kind::Image),
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
                original_name TEXT,
                kind TEXT,
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
                                     created_at, original_name, kind, deleted_at, purged_at, pinned)
             SELECT id, source_format, bytes, sha256, width, height,
                    created_at, original_name, kind, deleted_at, purged_at, pinned
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

        let trashed = store.trashed_assets(200, 0).await.unwrap();
        assert_eq!(store.trashed_count().await.unwrap(), 2);
        assert_eq!(trashed.len(), 2);
        assert_eq!(trashed[0].asset.id, purged, "newest deletion first");
        assert_eq!(trashed[0].asset.state(), AssetState::Purged);
        assert_eq!(trashed[1].asset.id, deleted);
    }
}

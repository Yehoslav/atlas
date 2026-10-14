-- Base schema.
--
-- These statements are deliberately idempotent so that a database created by a
-- pre-migration build adopts this migration instead of failing to apply it.

CREATE TABLE IF NOT EXISTS assets (
    id TEXT PRIMARY KEY NOT NULL,
    source_format TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    width INTEGER,
    height INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS renditions (
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    max_width INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    format TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (asset_id, max_width, format)
);

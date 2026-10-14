-- Trash lifecycle: soft delete, purge tombstones, and consumer usage pins.
--
-- Additive only. `ALTER TABLE ... ADD COLUMN` rewrites no rows and fires no
-- cascades, so this is safe to apply to a live database.

ALTER TABLE assets ADD COLUMN deleted_at INTEGER;
ALTER TABLE assets ADD COLUMN purged_at INTEGER;
ALTER TABLE assets ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS assets_by_deleted_at ON assets (deleted_at);

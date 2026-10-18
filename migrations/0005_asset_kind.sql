-- Which collection an asset was uploaded into.
--
-- Kind is the caller's choice (the URL they posted to), not a property of the
-- bytes: a PNG can live in either collection and is stored the same way in
-- both. It decides which operations exist, whether renditions are generated,
-- and how the file is served.
--
-- The backfill cannot use "is this an image format", because that is the
-- *format* layer, and a document collection holds images too. What it can use
-- is what an existing row actually was: every row written so far was uploaded
-- to the single collection, and it generated renditions exactly when its
-- format was convertible.
--
-- Additive only: nullable, backfilled, never rebuilt.

ALTER TABLE assets ADD COLUMN kind TEXT;

UPDATE assets
SET kind = CASE WHEN source_format IN ('jpeg', 'png', 'webp') THEN 'image'
                ELSE 'document' END;

CREATE INDEX IF NOT EXISTS assets_by_kind ON assets (kind, created_at);
CREATE INDEX IF NOT EXISTS assets_by_sha256 ON assets (sha256);

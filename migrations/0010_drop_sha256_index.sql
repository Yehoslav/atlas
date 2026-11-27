-- Drop the single-column index the composite supersedes.
--
-- 0009 added `(sha256, kind)`. Its leading column answers a pure-hash filter
-- on its own, so the `assets_by_sha256` index from 0005 no longer serves a
-- read that the composite does not. Keeping it only adds a second b-tree to
-- update on every insert.
--
-- 0009's comment is right that the old index still works for a hash-only
-- filter; it is just no longer needed for one. Nothing else in Atlas queries
-- `sha256` without `kind`.
--
-- No data changes: an index is derived from the rows, and recreating it is
-- enough to undo this.

DROP INDEX IF EXISTS assets_by_sha256;

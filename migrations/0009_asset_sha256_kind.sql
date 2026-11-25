-- Look up assets by content hash, narrowed by collection.
--
-- Dedup keys on `(sha256, kind)`: identical bytes mean different things in
-- different collections, and a batch hash lookup asks both questions at once.
-- The single-column index from 0005 still serves a pure-hash filter; this one
-- covers the pair without a table scan as the library grows.
--
-- Additive only: no column changes, no rebuild.

CREATE INDEX IF NOT EXISTS assets_by_sha256_kind ON assets (sha256, kind);

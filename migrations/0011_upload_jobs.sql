-- Asynchronous upload conversion (issue #30).
--
-- Additive: one new table. A row is an upload whose intake finished — bytes
-- streamed, signature and frame count checked, hash taken — and whose
-- conversion runs detached from the request. The original waits in
-- `uploads/{id}/` until the job is terminal, so a restart converts it again
-- rather than failing the client.
--
-- `asset_id` is the id the `202` handed out; it is reserved here, not in
-- `assets`, so it reads as 404 everywhere until the asset row is committed
-- together with its renditions. `result_id` is the asset a `ready` job
-- resolved to: the reserved id, or an older asset with the same bytes when
-- the promotion deduplicated.
CREATE TABLE IF NOT EXISTS upload_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    asset_id TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    source_format TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    original_name TEXT,
    result_id TEXT,
    deduplicated INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    error_message TEXT
);

CREATE INDEX IF NOT EXISTS upload_jobs_state ON upload_jobs (state);

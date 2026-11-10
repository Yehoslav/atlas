-- Asynchronous re-render jobs (issue #15).
--
-- Additive: two new tables. A job snapshots its selection into items at
-- creation, so progress totals are exact and results are durable rows rather
-- than process memory. Items are the source of truth; the job row carries only
-- lifecycle and the job-level error, so counters cannot drift from outcomes.

CREATE TABLE IF NOT EXISTS re_render_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    error TEXT
);

-- `ON DELETE CASCADE` here is the prune path: deleting a job row removes its
-- items in the same statement. The guard in migrations/README.md is about
-- rebuilding tables that already hold child rows, not about adding new ones.
CREATE TABLE IF NOT EXISTS re_render_job_items (
    job_id TEXT NOT NULL REFERENCES re_render_jobs(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    id TEXT NOT NULL,
    expect_generation INTEGER,
    outcome TEXT NOT NULL,
    generation INTEGER,
    error_code TEXT,
    error_message TEXT,
    PRIMARY KEY (job_id, position)
);

CREATE INDEX IF NOT EXISTS re_render_jobs_state ON re_render_jobs (state);

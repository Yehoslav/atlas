-- Let a rendition be re-encoded without changing what its URL means.
--
-- An encoding that turned out to be bad was permanent: a rendition's bytes are
-- fixed at upload, and a public URL that changes its bytes is a lie to every
-- cache that holds it. A generation makes the promise precise instead of
-- absolute. Rendition URLs name the generation they were handed out for, the
-- bytes behind a generation never change, and a re-encode writes a new
-- generation beside the old one.
--
-- `renditions` keeps holding the asset's current generation, so every reader
-- that already exists keeps reading the right rows. A re-encode moves the rows
-- it supersedes into `superseded_renditions`, which is what an older
-- generation's URL is resolved against. The files stay where they are.
--
-- Additive only: a new column with a constant default and a new table. The
-- primary key of `renditions` cannot grow a generation without rebuilding the
-- table, and a second table is the change that rebuilds nothing.

ALTER TABLE assets ADD COLUMN generation INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS superseded_renditions (
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    max_width INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    format TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    file_name TEXT NOT NULL,
    PRIMARY KEY (asset_id, generation, max_width, format)
);

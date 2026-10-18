-- The album's own content. Atlas owns the media bytes; everything a person
-- would recognise about a photo - what it is of, where it belongs, what it
-- means - lives here and only here.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS folders (
    id       INTEGER PRIMARY KEY,
    name     TEXT    NOT NULL UNIQUE,
    position INTEGER NOT NULL DEFAULT 0
);

-- asset_id is the only link to Atlas. It is Atlas's public URL component, so
-- this row is what turns an opaque ID into an <img> tag.
CREATE TABLE IF NOT EXISTS photos (
    id           INTEGER PRIMARY KEY,
    asset_id     TEXT    NOT NULL UNIQUE,
    -- Which Atlas collection this asset is in, spelled as the route spells it.
    -- Atlas's routes are typed, so the album has to remember this beside the id
    -- rather than guess it from the content type: an image can live in either
    -- collection.
    collection   TEXT    NOT NULL DEFAULT 'images',
    description  TEXT    NOT NULL DEFAULT '',
    alt_text     TEXT    NOT NULL DEFAULT '',
    tags         TEXT    NOT NULL DEFAULT '',
    folder_id    INTEGER REFERENCES folders (id) ON DELETE SET NULL,
    content_type TEXT    NOT NULL,
    bytes        INTEGER NOT NULL,
    width        INTEGER,
    height       INTEGER,
    uploaded_at  TEXT    NOT NULL
);

-- The widths Atlas actually generated for this photo, copied from the upload
-- response. The album needs them to write a correct srcset without asking Atlas
-- about every image on every page load.
--
-- `max_width` is the profile that was requested; `width` is what came out, which
-- is smaller when the photo is narrower than the profile. Atlas never upscales,
-- and a srcset descriptor has to be `width`.
CREATE TABLE IF NOT EXISTS renditions (
    asset_id   TEXT    NOT NULL REFERENCES photos (asset_id) ON DELETE CASCADE,
    max_width  INTEGER NOT NULL,
    width      INTEGER NOT NULL,
    height     INTEGER NOT NULL,
    formats    TEXT    NOT NULL,
    PRIMARY KEY (asset_id, max_width)
);

CREATE INDEX IF NOT EXISTS photos_by_folder ON photos (folder_id);

-- A post is the thing that makes an asset "in use". Atlas cannot know about
-- these; this is why the album checks them before it deletes anything.
CREATE TABLE IF NOT EXISTS posts (
    id           INTEGER PRIMARY KEY,
    title        TEXT    NOT NULL,
    body         TEXT    NOT NULL DEFAULT '',
    published_at TEXT,
    created_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS post_photos (
    post_id  INTEGER NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    photo_id INTEGER NOT NULL REFERENCES photos (id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (post_id, photo_id)
);

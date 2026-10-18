-- Record where each rendition's bytes actually live.
--
-- Generated files are named after the *actual* output width, which is capped
-- by the source width, while `max_width` records the requested profile. A 500
-- pixel photo asked for `w960` is stored as `w500.*` under the profile `960`.
--
-- The backfill therefore names rows with `width`, the actual output width,
-- which is exactly how every file written so far was named. `max_width` would
-- point small photos at a file that does not exist.
--
-- Additive only.

ALTER TABLE renditions ADD COLUMN file_name TEXT;

UPDATE renditions
SET file_name = 'w' || width || '.' ||
    CASE format
        WHEN 'jpeg' THEN 'jpg'
        WHEN 'png' THEN 'png'
        WHEN 'webp' THEN 'webp'
        WHEN 'avif' THEN 'avif'
        WHEN 'jxl' THEN 'jxl'
    END;

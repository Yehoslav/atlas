-- Keep the name the uploader gave the file.
--
-- Without it every download is `asset.pdf`, and the name is gone for good once
-- the upload returns. Atlas stores it as a fact about the original; it is never
-- used as a path, so a hostile name cannot reach the filesystem.
--
-- Additive only, and nullable: uploads without a name are unchanged.

ALTER TABLE assets ADD COLUMN original_name TEXT;
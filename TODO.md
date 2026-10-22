# TODO

## Example consumer: Family Album

A small Flask application that consumes Atlas the way a real site would: it owns
its own content database (descriptions, alt text, tags, folders) and delegates
every media byte and every image transformation to Atlas. It exists to prove the
integration contract from the consumer side, so it is a *reference
implementation* - readable source, `requirements.txt`, and a README, with no
Nix integration and no production ambitions.

Placed at `examples/family-album/`.

### Prerequisites in Atlas

- [x] **Adopt sqlx migrations.** Enable the `migrate` feature and move the
      `CREATE TABLE IF NOT EXISTS` blocks in `Store::connect` into a
      `migrations/` directory applied with `sqlx::migrate!().run(...)` at
      startup. No new dependencies; `_sqlx_migrations` tracks checksums, so an
      edited migration is caught instead of silently ignored. Note that the
      macro does not rebuild when only `.sql` files change - add
      `sqlx migrate build-script` or use `RUSTFLAGS='--cfg sqlx_macros_unstable'`.
      Done, plus a `build.rs` that embeds the migration file list into
      `OUT_DIR`: `include_str!` already covers edits to existing files, and the
      generated list covers a *new* file appearing, which neither
      `sqlx migrate build-script` nor `sqlx_macros_unstable` guarantees on
      stable Rust.
- [x] **Know the `ALTER TABLE` limits.** SQLite supports only rename table,
      rename column, add column, and drop column directly. Changing a column
      type or a constraint requires the 12-step rebuild. Document this before
      anyone reaches for `ALTER TABLE ... ALTER COLUMN`.
- [x] **Guard table rebuilds against CASCADE.** `DROP TABLE` fires
      `ON DELETE CASCADE`, so rebuilding `assets` would silently delete every
      `renditions` row - and the database would look self-consistent afterward.
      Any rebuild must set `PRAGMA foreign_keys = OFF` *outside* the transaction
      (it is a no-op inside one), run the rebuild, then `PRAGMA foreign_key_check`
      and roll back on violations.
      Two tests pin this down: the guarded rebuild keeps `renditions`, and the
      same rebuild without the guard is shown to delete them with no error and
      no foreign key violation left behind.
- [x] **Prefer additive migrations.** Add nullable columns, backfill, deploy the
      code, then drop the old ones in a later release. Never rename-and-rebuild
      in a single step.
- [x] **Trash instead of hard delete.** Add `deleted_at` and `pinned` columns.
      `DELETE /api/v1/assets/{id}` moves `assets/<id>` to `trash/<id>` and stamps
      `deleted_at`. Add `POST /api/v1/assets/{id}/restore` and `GET /api/v1/trash`
      (authenticated). Public delivery stops serving the real bytes immediately,
      so a blog never shows a half-deleted asset.
- [x] **Tombstones.** On purge, keep the metadata row with `purged_at` so Atlas
      can tell "deleted" from "never existed". Unknown IDs stay `404`; known but
      deleted IDs serve the placeholder.
- [x] **Purge.** Remove `trash/<id>` and mark the row purged once past
      `trash_retention_days` (default 30, configurable). Run at startup and
      opportunistically on upload, throttled. No scheduler, no job queue.
      Startup recovery must leave `trash/` alone.
- [x] **Placeholder delivery.** Embed a small placeholder image in the binary;
      serve it for trashed/purged IDs with `Cache-Control: public, max-age=60`.
      Deliberately *not* immutable - an immutable placeholder could outlive a
      restore by a year.
- [x] **Usage registration.** Let a consumer mark an asset in use so purge skips
      it. Pins are Atlas's only hint about consumer content, so keep this minimal
      and unopinionated.
- [x] **Report state.** `GET /api/v1/assets/{id}` should expose deleted/pinned
      state so consumers can distinguish a missing asset from a deleted one.

### Database safety

- [x] **Startup integrity checks.** Run `PRAGMA integrity_check` and
      `PRAGMA foreign_key_check` after migrations, and log loudly on violations.
- [x] **Backup/restore runbook.** Document `VACUUM INTO` for a consistent
      snapshot of the live database (it also compacts, unlike `.backup`), never
      `cp` a live SQLite file, and note that SQLite does not `fsync` the
      `VACUUM INTO` output - so a restore drill must verify the copy rather than
      assume it worked.

### The example app

- [x] **Scaffold.** `examples/family-album/` with Flask, `requirements.txt`, a
      README, and its own `.gitignore`.
- [x] **Its own schema.** SQLite tables for photos (Atlas asset id, description,
      alt text, tags, folder) and folders. This database is the point: Atlas owns
      bytes, the app owns meaning.
- [x] **Atlas client.** Server-side HTTP client for `ATLAS_API_TOKEN`. The token
      never reaches the browser; browsers only ever talk to Flask.
- [x] **Upload, single and bulk.** One file at a time, sequentially - Atlas
      allows two concurrent uploads and one conversion, so a burst risks `503`.
      Bulk upload reports partial success per file.
- [x] **Metadata editing.** Description, alt text, tags, and folder assignment;
      changing these touches only the app's own database.
- [x] **Delete with a referential guard.** Before deleting, the app checks its
      own database for published posts referencing the asset and blocks or warns.
      This is the safety net Atlas cannot provide on the app's behalf.
- [x] **Use the renditions properly.** `w320` for folder thumbnails, `w640` and
      `w960` for grid images via `srcset`/`sizes`, `w1280` or the original for the
      full-size view. Store the rendition list returned by the upload response.
- [x] **Degrade gracefully.** If an asset `404`s or returns the placeholder, show
      a sensible empty state rather than a broken image.
- [x] **README.** Two-process setup (Atlas plus Flask), where the token lives, and
      which URLs are public.

### Explicitly out of scope

- Atlas-side listing, search, or tags - the consumer's own database covers that,
  and the separation is the point.
- Authentication or multi-user support in the example app.
- Anything that would push Atlas toward knowing about consumer content.

## Open threads

### For the first real consumer

- [x] **Name a rendition format in its public URL.** A crawler fetching an
      image for a marketplace cannot use `Accept` to choose a format, because
      the crawler's header describes what the crawler can fetch, not what the
      marketplace accepts. `/media/{id}/w800?format=webp` selects an already
      generated representation, overrides `Accept`, and omits `Vary: Accept`.
      Unknown formats are `400`; formats the asset does not have are `404`.
- [x] **Keep JPEG XL faithful to its source.** ImageMagick's JXL coder turns
      `-quality` into a libjxl distance, where higher means more loss. The
      quality needed by AVIF leaked into the JXL write and degraded product
      photos. Write JXL before setting any quality, and test a per-format PSNR
      floor against a checked-in reference image so a future encoder change
      cannot silently degrade every rendition.
- [x] **Make rendition configuration discoverable and repairable.** Report
      configured widths in the summary, make `srcset` default to those widths,
      and add an authenticated operation that adds missing rendition profiles
      without changing the asset ID or replacing bytes behind an existing URL.
      Profiles that resolve to the same actual width share the stored file;
      the additional profile rows point at it rather than re-encoding over it.
- [x] **Correct a bad encoding without a new asset id.** Rendition URLs
      carry a generation (`?g=2`); `{"re_encode": true}` on the renditions
      operation writes the next generation from the original beside the old
      files, and `POST /api/v1/images/re-render` does it for every live image
      a page at a time. A generation URL is immutable; the bare URL follows
      the current generation with a one-hour cache. The re-encode is held to
      the upload's fidelity floors.
- [x] **Ship a deployment path.** Include a systemd unit, an ImageMagick
      container image, and a deployment runbook covering the environment file,
      persistent storage, proxy, health check and upgrade order. Build the
      image in CI and publish tagged images to GHCR; a one-core, one-gigabyte
      service host should pull an image rather than compile the Rust tree.

### Needs a real file, not a test

- [x] **Verify wide-gamut renditions.** The flag only tagged. ImageMagick
      reports a Display P3 photo as sRGB already, so `-colorspace sRGB` left
      its numbers alone and `-strip` threw away the profile that gave them
      their meaning: WebKit's Display P3 sample photos came out pixel for
      pixel identical to their P3 values, 11–16% less saturated than an ICC
      conversion. Renditions now go through `-profile` with a compact sRGB
      profile embedded in the binary, and startup refuses an ImageMagick
      without LCMS, where `-profile` is only a warning nobody reads. A test
      uploads a pixel tagged with `tests/fixtures/DisplayP3-v4.icc`.

### For the CMS admin dashboard example

- [x] **`kind` in the trash listing.** `GET /api/v1/trash` now names the
      collection each entry is in, so a consumer listing the whole trash can
      address a per-entry restore. The CMS Admin example exercises it with an
      image stored as a document, where the content type alone would point at
      the wrong collection.

### HEIC image sequences

- [ ] **Refuse the HEVC sequence brands.** The byte check accepts the brands
      `hevc`, `hevx`, `hevm` and `hevs`, and IANA maps exactly those four to
      `image/heic-sequence` — the media type Atlas already refuses at the
      door. A sequence is a grid of pictures, not a picture, and Atlas keeps
      one original and generates renditions from one frame of it. Drop those
      four brands, keep `heic`, `heix`, `heim` and `heis`, add a test that a
      sequence-branded file is refused, and say in the README and the plan
      that a sequence is a `415` now. It turns a `201` into a `415`, so it
      wants a decision of its own rather than riding along with a fix.

## Things that got through, and why

Worth keeping because each shipped green and each was found by hand:

- **A fake is shaped like what you hope, not like the server.** `FakeAtlas`
  answered `kind` as `"images"` where the server says `"image"`, so every real
  delete, pin and restore would have 404'd while 40 tests passed. Point the real
  client at the real binary (`scripts/smoke.py`) when the contract changes.
- **A code path with no test is a code path with a bug.** The index's folder and
  tag filters were broken for the life of the feature: adding the deleted-photo
  clause produced two `WHERE` clauses, and no test had ever filtered the index.
  When a query or a branch is added, test it *with* its filters.
- **A helper with two callers needs two answers, not one default.**
  `references_for_photo` filtered to published posts by default, which the
  delete guard wanted and the "Used by" panel did not. The template even carried
  a draft badge for a case it could never receive.
- **Check the sibling queries.** The folder count in the sidebar kept counting
  soft-deleted photos, one clause below the query that had just been changed.

# Atlas

Atlas is an API-first media service for small sites. It stores media files,
generates a limited set of image renditions, and delivers public assets. Each
site runs its own Atlas instance alongside its application; the application
keeps its own structured content and database.

The prototype supports authenticated backend/CLI uploads and public reads. It
is designed for occasional administrator uploads and mostly blog-style media,
not as a complete CMS. Image uploads must be web-ready JPEG, PNG, or WebP;
camera RAW formats are rejected. PDF and Word documents are stored and served
without conversion.

## Implemented prototype API

Every route below that reads, changes, or renders an asset is *typed*: the
collection is part of the path, `images` or `documents`. A mistake is a `404`
rather than a silent no-op, and a reader can see which collection a call acts
on. Only the two mixed-set endpoints (`state` and `trash`) are untyped, because
they exist precisely to span collections.

An asset's **kind** is what it is used for — `images` get renditions,
`documents` do not — while its **format** is its media type: a JPEG, a PNG, a
PDF. The two are independent. `POST /api/v1/documents` accepts any allowlisted
format, images included, so a diagram or a scan can be stored whole and served
as an attachment instead of being re-encoded into a picture.

- `GET /healthz` — local process health.
- `POST /api/v1/images` — authenticated image upload. The body is the file
  itself; `Content-Type` must be `image/jpeg`, `image/png`, or `image/webp`, and
  the optional `X-Original-Filename` header names it. Streams to disk, generates
  the configured renditions, and returns an asset ID, original metadata, and
  rendition URLs.
- `POST /api/v1/documents` — the same upload for `documents`. `Content-Type` may
  be any allowlisted format, images included. The file is stored and served
  untouched, with no renditions.
- `GET /api/v1/{kind}` — authenticated listing of that collection, paged with
  `limit` (default 50, max 200) and `offset`, and returning `total`. Also
  accepts `state=active|deleted|purged|all` (default `active`), a `q` substring
  of the original filename, the ranges `created_after`, `created_before`,
  `bytes_min`, `bytes_max`, `width_min`, `height_min`, `sort` of `created_at`,
  `bytes`, `width` or `filename`, and `order=asc|desc`. `deleted` and `purged`
  are different answers: a deleted asset can still be restored, a purged one
  cannot. An unknown value is a `400 invalid_sort`/`invalid_state` rather than a
  quietly ignored filter. This is a listing of assets the site uploaded; it says
  nothing about how the site uses them.
- `GET /api/v1/{kind}/{id}` — authenticated metadata lookup, including the
  asset's `kind`, `state` (`active`, `deleted`, or `purged`), and `pinned` flag.
- `POST /api/v1/{kind}/actions` — authenticated bulk action for ids the caller
  already has, such as `pin`, `unpin`, `delete`, or `restore`. Body
  `{"action": "pin", "ids": ["…"]}`, max 500 ids. Validation is all-or-nothing:
  one unknown id, a duplicate, or an action the current state does not allow
  rejects the whole batch with `400` and changes nothing.
- `GET /api/v1/summary` — authenticated counts and stored bytes for the whole
  instance, including the trash. A cheap dashboard number, not a listing.
- `POST /api/v1/assets/state` — authenticated batch state lookup for ids the
  caller already has, across both collections. Not a listing.
- `DELETE /api/v1/{kind}/{id}` — authenticated move to the trash. Returns `204`.
- `POST /api/v1/{kind}/{id}/restore` — authenticated restore from the trash.
  Returns `204`, or `409` if the asset was already purged.
- `PUT /api/v1/{kind}/{id}/pin` — authenticated usage registration. Body
  `{"pinned": true|false}`; purge skips pinned assets.
- `GET /api/v1/trash` — authenticated listing of trashed and purged assets from
  both collections, paged with `limit` (default 200, max 500) and `offset`.
- `GET /media/{id}/original` — public original delivery.
- `GET /media/{id}/w{width}` — public width rendition. Atlas selects a
  pre-generated JXL, AVIF, WebP, or JPEG/PNG fallback using `Accept` and returns
  `Vary: Accept`.
- `GET /api/v1/images/{id}/srcset` — authenticated ready-made `srcset` for one
  image, one candidate per stored width. Each URL is the profile path and each
  descriptor is the width that was really produced, so a photo narrower than
  its largest profile is described honestly. Errors on an unknown asset
  (`404`), a non-image, or a width Atlas does not have.

Uploads are content-addressed within their collection. If the exact bytes are
already live there, nothing is written or converted and the response is `200`
with the existing asset and `"deduplicated": true`. A duplicate is not an
error, and the alternative is paying for a conversion and storing the result
twice.

The default rendition widths are 320, 640, 960, and 1280 pixels. They preserve
the original aspect ratio and are not upscaled. Configure a different fixed
set with `ATLAS_VARIANT_WIDTHS=320,640,1280`; arbitrary sizes are not generated
on public requests. Consuming sites can fetch `/api/v1/images/{id}/srcset` and
set their own `sizes` value. Atlas does not provide search, and never converts
or re-encodes on a read.

## How a rendition URL resolves

`/media/{id}/w960` asks for an image **no wider than** 960 pixels, not for a
particular file. Atlas serves the rendition stored under the `960` profile when
there is one, and otherwise the widest stored rendition that is not wider than
the request.

That distinction matters for small photos. A 500-pixel photo asked for `w960`
has no 960-pixel rendition, because Atlas does not upscale, so it is answered
with its 500-pixel rendition rather than a blank response. Metadata reports the
two separately: `max_width` is the requested profile, `width` is what was
actually produced. Describe a stored rendition in `srcset` with `width` — the
real output width — while keeping the URL pointed at the profile:

```html
<!-- A 500-pixel photo stored under profiles 320, 640, 960 and 1280 -->
<img src="/media/4f2k9xq7m1b8w3z0pd/w960" srcset="/media/4f2k9xq7m1b8w3z0pd/w320 320w, /media/4f2k9xq7m1b8w3z0pd/w640 500w" sizes="640px" alt="…">
```

Asking for something narrower than anything stored is a `404` with
`rendition_not_found`, listing the widths that do exist. That is deliberately
different from `asset_not_found`, which means Atlas has never heard of the ID:

```sh
curl http://127.0.0.1:3000/media/4f2k9xq7m1b8w3z0pd/w64
{"error":{"code":"rendition_not_found","message":"The asset has no rendition at or below 64 pixels; stored widths are 320, 640, 960, 1280."}}
```

## Asset IDs

Asset IDs are random lowercase base36 strings, 18 characters by default
(`4f2k9xq7m1b8w3z0pd`). They carry about 93 bits of entropy, drawn from the
operating system's random source. With that entropy the odds of a collision
remain negligible until roughly 10¹⁴ assets, far beyond one site's needs.

IDs are opaque and generated by the server. Do not parse them, derive meaning
from them, or treat them as a secret — public media is public by design.
Because they are lowercase, they survive mail clients and proxies that
normalize path case; only rely on standard, case-preserving path routing.

`ATLAS_ID_LENGTH` changes the length between 12 and 24 characters. Existing
assets keep their original IDs, so the setting can be changed without a
migration.

Use a width rendition for displayed images; the original URL always serves the
uploaded format. For example:

```html
<img
  src="/media/4f2k9xq7m1b8w3z0pd/w640"
  srcset="/media/4f2k9xq7m1b8w3z0pd/w320 320w, /media/4f2k9xq7m1b8w3z0pd/w640 640w"
  sizes="(max-width: 640px) 100vw, 640px"
  width="640"
  height="853"
  alt="Description owned by the consuming site"
/>
```

Uploads require `Authorization: Bearer $ATLAS_API_TOKEN`. Keep this token in the
consuming server or CLI, never in browser code. Public media reads do not need a
token.

## Uploading a file

The body is the file. The collection in the path decides what happens to it,
`Content-Type` names the allowed format, and the optional
`X-Original-Filename` header names the file for downloads:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/pdf' \
  -H 'X-Original-Filename: menu-2026.pdf' \
  --data-binary @menu-2026.pdf \
  http://127.0.0.1:3000/api/v1/documents
```

A picture goes to `/api/v1/images`, which generates the configured renditions:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: image/png' \
  -H 'X-Original-Filename: diagram.png' \
  --data-binary @diagram.png \
  http://127.0.0.1:3000/api/v1/images
```

That same diagram to `/api/v1/documents` instead is stored exactly as uploaded,
generates no renditions, and is served as an attachment. Both are legitimate;
choose the collection by whether the picture needs to be *displayed* or
*preserved*.

`201` means a new asset was created. `200` means an asset with the same bytes
already existed in that collection and this is that asset — identical content is
the same asset, because URLs are immutable and content is what identifies one. A
duplicate costs a hash rather than an image conversion, and a retry after a
timeout gets the same asset instead of a second copy.

The name is optional metadata and is never used as a path: the file on disk is
named after the asset ID and the detected format. It arrives as raw UTF-8 or
percent-encoded, so `Отчет за 2026.pdf`, `Jahresbericht (Ü).pdf` and
`100% pure.pdf` all survive. Names are truncated at 255 characters, any path is
stripped, and a name containing control characters is refused with
`400 invalid_filename`. Delivery sends it as `Content-Disposition: inline;
filename="…"; filename*=UTF-8''…`, falling back to `asset.pdf` when the upload
had no name. Consumers that want a different display name should keep it in
their own database.

## Asking what Atlas can still serve

A consumer that owns its own database still has to ask Atlas whether the assets
it points at are still there, because Atlas forgets files without telling
anyone. Asking once per asset is what makes a gallery page expensive, so:

```sh
curl -X POST http://127.0.0.1:3000/api/v1/assets/state \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"ids":["8odfjrxxjxzypa99j6","aaaaaaaaaaaaaaaaaaaa"]}'
```

```json
{"assets":[
  {"id":"8odfjrxxjxzypa99j6","state":"active","pinned":true},
  {"id":"aaaaaaaaaaaaaaaaaaaa","state":"missing","pinned":false}
]}
```

One call covers up to 500 ids. Ids Atlas has never heard of come back as
`missing` rather than being absent, because telling "never existed" apart from
"deleted" is the whole reason to ask. A malformed id is a `400`, not a silent
omission. This is not a listing: the caller supplies the ids, so Atlas learns
nothing about what they are used for and the result cannot be paged through.

## Trash, restore, and purge

Atlas never destroys an asset on request. `DELETE /api/v1/images/{id}` moves
`assets/<id>` to `trash/<id>` and stamps `deleted_at`. The soft delete is
recorded *before* the files move, so from the first moment of the delete the
public URLs stop serving the real bytes and answer with a small embedded
placeholder image (`assets/placeholder.png`, compiled into the binary) sent with
`Cache-Control: public, max-age=60`. That is deliberate: the placeholder is not
immutable, because a restore can bring the asset back, and an immutable
placeholder could outlive the restore by a year.

Once an asset has been in the trash for `ATLAS_TRASH_RETENTION_DAYS` (30 by
default) and no consumer has pinned it, purge discards the files and keeps the
row as a tombstone with `purged_at`. Atlas can then still tell "deleted" from
"never existed": an unknown ID is `404`, a tombstoned ID serves the placeholder,
and `GET /api/v1/images/{id}` reports `state: "purged"`. Rendition rows go with
the bytes, so a tombstone does not advertise URLs that would resolve to
nothing. Purge runs at startup and, at most once an hour, after an upload;
there is no scheduler and no job queue.

Pins are the one hint Atlas has about consumer content. Register an asset that
a published post still uses and Atlas will never purge it.

Nothing here is a replacement for a consuming site's own referential checks —
Atlas cannot know which of its assets your posts mention.

### If a consumer and Atlas have drifted

Atlas cannot repair itself: it has no listing, and it is not told when the
media directory and the database come back from different backups. The repair
is on the consuming side, and it is bounded:

1. Collect the asset IDs your database references.
2. Ask `POST /api/v1/assets/state` for them in batches.
3. Every `missing` ID is an asset that no longer exists and cannot be
   recovered; re-upload the file and repoint your references to the new ID.
4. Every `deleted` or `purged` ID is recoverable only while the bytes are in the
   trash; `POST /api/v1/images/{id}/restore` brings them back.

A media directory restored on its own will report every asset as `missing`,
which is the signal to stop serving those URLs rather than a silent failure.

## Development

Install Nix with flakes enabled, then enter the pinned development environment:

```sh
nix develop
```

The shell provides Rust, Cargo, rustfmt, Clippy, rust-analyzer, SQLite,
ImageMagick, and build tools. `flake.lock` pins Nixpkgs. On startup, Atlas checks
that ImageMagick can read/write JPEG, PNG, WebP, AVIF, and JPEG XL. The API
still accepts only its explicit input allowlist.

Run locally:

```sh
export ATLAS_API_TOKEN='replace-with-a-long-random-secret'
export ATLAS_DATA_DIR="$PWD/data"
cargo run
```

> **Migrating from the UUID prototype:** asset IDs changed from UUIDs to short
> base36 strings. Assets created by an earlier prototype build keep their UUID
> in the database and become unreachable. Delete the development data directory
> and start fresh: `rm -rf ./data`

The default listener is `127.0.0.1:3000`. Example PDF upload from another
terminal:

```sh
export ATLAS_API_TOKEN='same-secret-as-the-running-server'

curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/pdf' \
  --data-binary @report.pdf \
  http://127.0.0.1:3000/api/v1/documents
```

Useful checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Example consumer

[`examples/family-album/`](examples/family-album/) is a small Flask site that
consumes Atlas the way a real one would: it owns its own database of folders,
descriptions, alt text, tags, and posts, keeps the API token on the server,
checks its own posts before deleting, and renders the stored rendition widths
in `srcset`. Its own tests run against a fake Atlas:

```sh
cd examples/family-album
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt pytest
python -m pytest tests -q
```

## Configuration

Atlas reads settings from, in increasing order of precedence: built-in
defaults, an optional TOML file, then environment variables. Set
`ATLAS_CONFIG` to the file's path; without it the file is skipped.

Non-secret deployment shape belongs in the TOML file, which is safe to commit
and share across sites:

```toml
# atlas.toml
listen_addr = "127.0.0.1:3000"
data_dir = "/var/lib/atlas"

id_length = 18
variant_widths = [320, 640, 960, 1280]

max_image_bytes = 20971520
max_document_bytes = 52428800
max_image_pixels = 40000000
max_image_dimension = 12000

magick_bin = "magick"
magick_timeout_secs = 45
image_processing_timeout_secs = 120

trash_retention_days = 30
```

Unknown keys are rejected so typos surface immediately. Secrets stay in the
environment: `ATLAS_API_TOKEN` is deliberately not a file setting, and a config
file that tries to set it will be refused.

Environment variables override the file, which is useful for per-host values:

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATLAS_CONFIG` | unset | Path to a TOML configuration file |
| `ATLAS_API_TOKEN` | Required | Bearer token for uploads and metadata lookup |
| `ATLAS_LISTEN_ADDR` | `127.0.0.1:3000` | Bind address |
| `ATLAS_DATA_DIR` | `./data` | Private persistent state directory; keep it outside the public web root |
| `ATLAS_ID_LENGTH` | `18` | Asset ID length, 12–24 characters |
| `ATLAS_MAX_IMAGE_BYTES` | 20 MiB | Streaming image-upload limit |
| `ATLAS_MAX_DOCUMENT_BYTES` | 50 MiB | Streaming document-upload limit |
| `ATLAS_MAX_IMAGE_PIXELS` | 40,000,000 | Decoded image pixel limit |
| `ATLAS_MAX_IMAGE_DIMENSION` | 12,000 | Maximum image width or height |
| `ATLAS_VARIANT_WIDTHS` | `320,640,960,1280` | Fixed maximum-width profiles |
| `ATLAS_MAGICK_BIN` | `magick` | ImageMagick executable |
| `ATLAS_MAGICK_TIMEOUT_SECS` | 45 | Per ImageMagick invocation limit |
| `ATLAS_IMAGE_PROCESSING_TIMEOUT_SECS` | 120 | Total processing limit per image |
| `ATLAS_TRASH_RETENTION_DAYS` | 30 | Days a trashed asset is kept before purge |

Uploads are streamed to staging files; at most two uploads and one conversion
run concurrently. ImageMagick receives explicit input/output formats and
resource limits. Failed requests do not publish partial assets. On startup,
Atlas applies any pending schema migrations, runs `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` (refusing to start if either reports a problem),
removes incomplete staging data, and returns any interrupted delete or restore
to the state its row records.

Schema changes live in `migrations/` and run through `sqlx::migrate!`. See
[`migrations/README.md`](migrations/README.md) for the SQLite `ALTER TABLE`
limits, the `ON DELETE CASCADE` hazard when rebuilding a table, and the
additive-migration policy Atlas follows.

## Production notes

Production targets ordinary Linux hosts, not specifically NixOS. Use a dedicated
private persistent data directory with mode `0700`; Atlas rejects an existing
non-empty data directory that is accessible by group/other users. Install
SQLite runtime libraries and ImageMagick with the required codecs. Check codec
support with `magick -list format`. Run Atlas behind the site's HTTPS reverse
proxy, routing `/media/` publicly and keeping the token-protected API on the
server side. Proxy path matching must be case-preserving.

## Backup and restore

Metadata and files are useless separately: a restore that pairs a database with
a different set of `assets/` directories makes every asset undeliverable. Back
both up, and drill the restore.

**Snapshot the database with `VACUUM INTO`, never `cp`.** Atlas runs SQLite in
WAL mode, where committed transactions may still live in `atlas.sqlite3-wal`.
Copying `atlas.sqlite3` on its own captures neither the WAL nor a consistent
cut, and the result may be a database that opens fine and is missing rows.
`VACUUM INTO` takes a read lock, writes a single consistent file, and compacts
it as a side effect:

```sh
sqlite3 "$ATLAS_DATA_DIR/atlas.sqlite3" \
  "VACUUM INTO '/backup/atlas-$(date -u +%Y%m%dT%H%M%SZ).sqlite3'"
```

**SQLite does not `fsync` the `VACUUM INTO` output.** A crash or a full disk
during the write can leave a short file that still opens. A backup job that
assumes success is a backup that does not exist, so verify each snapshot:

```sh
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 'PRAGMA integrity_check;'
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 \
  'SELECT COUNT(*) FROM assets; SELECT COUNT(*) FROM renditions;'
```

`PRAGMA quick_check` is enough to catch a truncated file; `integrity_check` is
the stricter version. Keep the output, and compare the counts with the live
database (`SELECT COUNT(*) FROM assets`) at a quiet moment.

**Then copy the files.** A file copy only has to be internally consistent, so
`snapshot`, `rsync -a`, or `tar` are all fine:

```sh
rsync -a --delete "$ATLAS_DATA_DIR/assets/" /backup/assets/
rsync -a --delete "$ATLAS_DATA_DIR/trash/"  /backup/trash/
```

Snapshot the database *first*: the file copy taken afterwards may contain an
asset the database does not know about (harmless — startup recovery removes
unindexed directories) rather than missing one (broken delivery until the next
upload). Note that files copied while Atlas is running can be mid-upload; the
staging directory is never published, so ignore it and let startup recovery
clear it.

**Restore drill.** Stop Atlas, restore both halves, start it, and check:

1. `GET /healthz` returns `ok` — this already proves the migrations applied and
   both `PRAGMA` checks passed.
2. `GET /api/v1/assets/{id}` for a known asset returns its metadata.
3. `GET /media/{id}/original` returns `200` with the expected length, not a
   placeholder.

A restore drill that has not fetched an actual file has not tested the thing
that matters.

## License

Atlas is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).

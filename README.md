# Atlas

Atlas is an API-first media service for small sites. It stores media files,
generates a few fixed image renditions, and delivers public assets over HTTP.
Each site runs its own Atlas instance next to its own application; the
application keeps its own content and its own database.

*This project was written with the help of large language models. The design,
the code and the review are the author's.*

## Why Atlas exists

For a site built from the ground up, the media part is the same every time: an
upload endpoint that validates and limits the file, a handful of image sizes, a
way to find out whether last month's file is still there, and a trash that does
not break the pages already pointing at it. Off-the-shelf CMSs covered that
ground, but for the scope of these projects — usually student NGOs — they were
either more than the budget or more than the appetite. So the part that was
genuinely common got written down once, and the part that differs — content,
structure, meaning — stayed with each site.

## Why a service and not a library

A library would have to be compiled into the consuming application, which ties
every project to one language's ecosystem and one release cadence. Atlas is
reached over HTTP, so the consuming application can be written in anything: the
two examples in this repository are one Python and one Rust, and neither knows
anything about the other beyond the API. The cost is a process to run, a token
to manage and a database to back up; the gain is that the media limits, storage
rules and lifecycle are shared rather than reimplemented per project.

## How it is deployed

One Atlas instance per site, on the same host as the site, with its data
directory on local persistent storage. The API needs a bearer token, which
stays on the server or in a CLI and never reaches browser code; the media URLs
under `/media/` are public and served by a reverse proxy in front of both
processes. Metadata lives in SQLite next to the files, because the two are
useless separately.

## What it deliberately does not do, and why

| Not in Atlas | Why |
| --- | --- |
| No browser UI inside Atlas | The API token is permanent, and a browser-facing upload needs a grant flow. An operator interface belongs to the consuming site's own application — `examples/cms-admin/` is one, written as an ordinary consumer. |
| No camera RAW | RAW decoders are the least reliable corner of ImageMagick, and a half-decoded RAW is a permanently broken original. Convert first. |
| No upscaling; fixed width profiles | Resizing on demand is a CPU denial-of-service and an unbounded storage multiplier. Fixed profiles are predictable, cacheable and enumerable. |
| No arbitrary transforms, no crop, no art direction | Every crop depends on the consuming site's layout, which Atlas does not know. Consumers supply their own `sizes` and `srcset`. |
| No content search; no meaningful tags | Atlas knows the bytes, not what they mean. Captions, alt text and post relationships belong to the consumer. |
| No nested folders | A media service with a folder tree becomes a document management system. Flat opaque labels stop every project rebuilding label CRUD; the consumer keeps the hierarchy. |
| Deletion is a lifecycle, not an erasure | A published page must not break because someone deleted a file. Bytes move to a trash directory, are purged after a retention window, and a tombstone remains. |
| No video, no document rendering, no OCR | Different products. |
| No object storage, no multi-tenancy, no job queues | One site, one host, occasional administrator uploads. |
| No animations yet | The rendition model is one frame per width. A multi-frame kind needs a multi-frame model. |

## Try it

```sh
nix develop
export ATLAS_API_TOKEN='a-long-random-secret'
cargo run
```

Or put the settings in a file, so they do not have to be exported every time:

```sh
cp .env.example .env
$EDITOR .env
cargo run
```

In another shell:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: image/png' \
  --data-binary @photo.png \
  http://127.0.0.1:3000/api/v1/images
```

The response carries the asset id and the rendition URLs; `GET /media/{id}/w640`
serves the picture. The full set of formats Atlas accepts is in
`GET /api/v1/formats`, and it is exactly the set Atlas produces, so anything it
serves can be uploaded again — the file you have just downloaded is not a dead
end.

## How it is put together

- **Typed routes.** Every route that reads, changes or renders an asset names
  its collection in the path, `images` or `documents`. A mistake is a `404`
  rather than a silent no-op. Only the two mixed-set endpoints, `state` and
  `trash`, are untyped, because they exist precisely to span collections.
- **Kind and format are different things.** The *kind* is what an asset is for:
  images get renditions, documents do not. The *format* is its media type. The
  documents collection accepts images too, so a diagram can be stored whole and
  served as an attachment instead of being re-encoded into a picture.
- **Content-addressed originals.** Identical bytes are the same asset. An upload
  whose original matches a live asset returns that asset, costs a hash rather
  than a conversion, and makes a retry after a timeout idempotent.
- **Immutable public URLs.** Replacing bytes means uploading a new asset id, so
  a URL that is in a page cannot change meaning underneath it.
- **Deletion is a lifecycle.** Bytes move to a trash directory, are purged after
  a retention window, and leave a tombstone. A consumer can pin an asset it
  still uses, and that pin is the only hint Atlas ever gets about consumer
  content.
- **The token is the only coupling.** Public media reads need no token; every
  write needs one.

## Project layout

| Path | What is in it |
| --- | --- |
| `src/` | The service: `api.rs` (routes and responses), `store.rs` (SQLite), `media.rs` (ImageMagick), `model.rs`, `config.rs`, `ident.rs` (asset ids), `filename.rs` (upload names), `lib.rs` (wiring). |
| `migrations/` | Additive SQL migrations, applied at startup. `migrations/README.md` explains the SQLite constraints that shape them. |
| `examples/family-album/` | A Flask consumer with its own database: the integration contract from the site side. |
| `examples/cms-admin/` | A Rust consumer with no database at all: the same API seen from an operator's side. |
| `scripts/smoke.py` | End-to-end checks against the real binary and the real examples. |
| `assets/` | The placeholder served for retired assets, and the sRGB profile every rendition is converted to. |
| `tests/fixtures/` | A Display P3 profile, for the test that renditions convert colours rather than relabel them. |
| `PLAN.md` | The design record: the decisions, and the reasoning behind them. |
| `TODO.md` | What is open, and what has already been settled. |

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
  itself; `Content-Type` must be `image/jpeg`, `image/png`, `image/webp`,
  `image/avif`, `image/jxl` or `image/heic`, and
  the optional `X-Original-Filename` header names it. Streams to disk, generates
  the configured renditions, and returns an asset ID, original metadata, and
  rendition URLs.
  A HEIC photograph may also be declared as `image/heif`: freedesktop.org
  makes `image/heif` the canonical type for a `.heic` file and keeps
  `image/heic` as its alias, so a browser, a file manager and a command line
  disagree about which spelling to send. Either is accepted, the bytes decide
  what the file is, and the asset is stored and served as `image/heic`
  whichever spelling it arrived under. `image/heic-sequence` is not a picture
  and is not accepted.
- `POST /api/v1/documents` — the same upload for `documents`. `Content-Type` may
  be any allowlisted format, images included. The file is stored and served
  untouched, with no renditions.
- `GET /api/v1/{kind}` — authenticated listing of that collection, paged with
  `limit` (default 50, max 200) and `offset`, and returning `total`. Also
  accepts `state=active|deleted|purged|all` (default `active`), a `q` substring
  of the original filename, a `mime` filter, the ranges `created_after`,
  `created_before`, `bytes_min`, `bytes_max`, `width_min`, `height_min`, `sort`
  of `created_at`, `bytes`, `width` or `filename`, and `order=asc|desc`.
  `deleted` and `purged` are different answers: a deleted asset can still be
  restored, a purged one cannot. An unknown value is a `400 invalid_sort`/
  `invalid_state`/`invalid_mime` rather than a quietly ignored filter. This is
  a listing of assets the site uploaded; it says nothing about how the site
  uses them.

  `mime` takes an exact media type (`image/png`) or a whole group
  (`image/*`), and answers the question a collection cannot: `documents` holds
  PDFs, Word files and pictures stored whole, and `?mime=image/*` is what finds
  the last of those. A value Atlas does not accept, and a value this collection
  could never hold, are both `400 invalid_mime`; `GET /api/v1/formats` says
  which types are on offer.
- `GET /api/v1/{kind}/{id}` — authenticated metadata lookup, including the
  asset's `kind`, `state` (`active`, `deleted`, or `purged`), and `pinned` flag.
- `POST /api/v1/{kind}/actions` — authenticated bulk action for ids the caller
  already has, such as `pin`, `unpin`, `delete`, or `restore`. Body
  `{"action": "pin", "ids": ["…"]}`, max 500 ids. Validation is all-or-nothing:
  one unknown id, a duplicate, or an action the current state does not allow
  rejects the whole batch with `400` and changes nothing.
- `GET /api/v1/summary` — authenticated counts and stored bytes for the whole
  instance, including the trash. A cheap dashboard number, not a listing.
- `GET /api/v1/formats` — authenticated list of the media types each collection
  accepts, the `mime` groups they answer to, and whether they get renditions.
  Derived from the same table the upload path validates against, so it cannot
  advertise something Atlas would then refuse. A consumer building an upload
  form should read this rather than hardcode a list that drifts.
- `POST /api/v1/assets/state` — authenticated batch state lookup for ids the
  caller already has, across both collections. Not a listing.
- `DELETE /api/v1/{kind}/{id}` — authenticated move to the trash. Returns `204`.
- `POST /api/v1/{kind}/{id}/restore` — authenticated restore from the trash.
  Returns `204`, or `409` if the asset was already purged.
- `PUT /api/v1/{kind}/{id}/pin` — authenticated usage registration. Body
  `{"pinned": true|false}`; purge skips pinned assets.
- `PUT /api/v1/{kind}/{id}/data` — authenticated consumer-owned note. Body
  `{"data": "..."}`, or `{"data": null}` to clear. Atlas stores it, caps it at
  `ATLAS_MAX_CONSUMER_DATA_BYTES` (64 KiB) and hands it back untouched: it does
  not read the value, validate its shape, search it or log it. Returned by
  `GET /api/v1/{kind}/{id}` and deliberately absent from listings, so a page of
  assets cannot grow into a page of blobs.
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
set their own `sizes` value. Atlas does not index media *content* — it filters
on filenames, media types and lifecycle state, and leaves meaning to the
consumer — and it never converts or re-encodes on a read.

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
that ImageMagick can read/write JPEG, PNG, WebP, AVIF, JPEG XL and HEIC, and
that it is built with LCMS, which converts a photo's colour profile to sRGB, and
refuses to start without them. The API still accepts only its explicit input
allowlist.

Run locally:

```sh
export ATLAS_API_TOKEN='replace-with-a-long-random-secret'
export ATLAS_DATA_DIR="$PWD/data"
cargo run
```

Or put those in a file instead, so they do not have to be exported every
time:

```sh
cp .env.example .env
$EDITOR .env
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

### End-to-end check against a real binary

The test suite drives the router in process, and the example's tests replace
Atlas with an in-memory fake. Neither can notice when the two disagree about the
API, which is a real risk when one side is written by the same hand as the other.
`scripts/smoke.py` starts the built binary on a throwaway data directory, checks
the HTTP contract over a real socket with real conversions, points the example
consumers' own clients at it, and always stops the servers afterwards:

```sh
cargo build
cargo build --manifest-path examples/cms-admin/Cargo.toml
nix develop --command python3 scripts/smoke.py
```

It exits non-zero on the first failed expectation, prints which checks were
skipped rather than reporting them as passes, and needs no arguments: the port,
binary, and data directory all have defaults that `--port`, `--bin` and `--data`
can override. It checks the HTTP contract, the album's real Python client, and
— when its binary exists — the CMS admin UI against the same real Atlas.

## Example consumer

[`examples/family-album/`](examples/family-album/) is a small Flask site that
consumes Atlas the way a real one would: it owns its own database of folders,
descriptions, alt text, tags, and posts, keeps the API token on the server,
checks its own posts before deleting, and renders the stored rendition widths
in `srcset`. Its own tests run against a fake Atlas:

```sh
cd examples/family-album
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements-dev.txt
python -m pytest tests -q
ruff check .
ruff format --check .
ty check
```

`requirements-dev.txt` adds the standalone test and lint tools. The repository's
Nix development shell also provides Python, Flask, requests, pytest, ruff and
ty at pinned versions, so `nix develop` is the reproducible path.

### CMS Admin

[`examples/cms-admin/`](examples/cms-admin/) is a second first-party consumer,
written in Rust, that exercises the same API from a different kind of site. It
is a standalone crate and does not depend on the Atlas crate: all reads and
mutations go through HTTP. It provides a password-protected media library with
search, filters, bulk actions, upload progress, asset detail, and the global
trash. Its Catppuccin Mocha / HTMX frontend has no Node build step; vendored
assets and their licences are recorded in the example.

```sh
nix develop
export ATLAS_API_URL='http://127.0.0.1:3000'
export ATLAS_API_TOKEN='the-same-long-random-secret-as-Atlas'
export ADMIN_PASSWORD='a-long-random-password'
cargo run --manifest-path examples/cms-admin/Cargo.toml
```

The dashboard is stateless. It deliberately does not claim to know what uses
an asset: only the consuming site's own content database can answer that.

## Configuration

Atlas reads its settings from four places. In increasing order of precedence:

1. built-in defaults
2. an optional TOML file, named by `ATLAS_CONFIG`
3. a local `.env` file
4. environment variables

A `.env` file is a convenience for development: copy `.env.example` to
`.env` and the settings stop having to be exported by hand. Its values only
fill in what the environment does not already provide, so a real environment
variable always wins over the file. Atlas looks for `.env` in the working
directory, or at the path in `ATLAS_ENV_FILE`; a missing file is not an error
and a malformed one is reported rather than half-read.

Non-secret deployment shape belongs in the TOML file, which is safe to commit
and share across sites. Secrets belong in the environment or in a `.env`
file, both of which are gitignored — never in the TOML, which is why
`ATLAS_API_TOKEN` is not a file setting and a config file that tries to set it
will be refused.

```toml
# atlas.toml
listen_addr = "127.0.0.1:3000"
data_dir = "/var/lib/atlas"

id_length = 18
variant_widths = [320, 640, 960, 1280]

max_image_bytes = 20971520
max_document_bytes = 52428800
max_consumer_data_bytes = 65536
max_image_pixels = 40000000
max_image_dimension = 12000

magick_bin = "magick"
magick_timeout_secs = 45
image_processing_timeout_secs = 120

trash_retention_days = 30
```

Unknown keys are rejected so typos surface immediately.

Environment variables override the file, which is useful for per-host values:

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATLAS_CONFIG` | unset | Path to a TOML configuration file |
| `ATLAS_ENV_FILE` | unset | Path to a `.env` file; defaults to `.env` in the working directory |
| `ATLAS_API_TOKEN` | Required | Bearer token for uploads and metadata lookup |
| `ATLAS_LISTEN_ADDR` | `127.0.0.1:3000` | Bind address |
| `ATLAS_DATA_DIR` | `./data` | Private persistent state directory; keep it outside the public web root |
| `ATLAS_ID_LENGTH` | `18` | Asset ID length, 12–24 characters |
| `ATLAS_MAX_IMAGE_BYTES` | 20 MiB | Streaming image-upload limit |
| `ATLAS_MAX_DOCUMENT_BYTES` | 50 MiB | Streaming document-upload limit |
| `ATLAS_MAX_CONSUMER_DATA_BYTES` | 64 KiB | Cap on a consumer's opaque `data` |
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
SQLite runtime libraries and ImageMagick with the required codecs and LCMS. Check
codec support with `magick -list format`, and LCMS on the `DELEGATES` line of
`magick -list configure`. Run Atlas behind the site's HTTPS reverse
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
2. `GET /api/v1/images/{id}` for a known asset returns its metadata.
3. `GET /media/{id}/original` returns `200` with the expected length, not a
   placeholder.

A restore drill that has not fetched an actual file has not tested the thing
that matters.

## Status

Atlas is a prototype, and it says so in its API. The HTTP contract is pre-1.0
and may change in ways that break a consumer; the schema only ever moves
forward, additively, but the routes and the responses are not frozen.

The design record is [`PLAN.md`](PLAN.md), which holds the decisions and the
reasoning behind them, and [`TODO.md`](TODO.md), which holds what is still open
and what has already been settled. Both are worth reading before changing
anything structural.

## License

Atlas is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).

The example consumers are part of this repository and carry the same licence.
The vendored front-end assets keep their own: HTMX is Zero-Clause BSD and the
Catppuccin palette is MIT, and both licence files sit beside the files
themselves. The two ICC profiles, `assets/sRGB-v4.icc` and
`tests/fixtures/DisplayP3-v4.icc`, are from
[Compact ICC Profiles](https://github.com/saucecontrol/Compact-ICC-Profiles)
and are released under CC0.

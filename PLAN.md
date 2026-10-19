# Atlas plan and prototype status

## Goal

Atlas is a small API-first media service for blog-like sites with little
user-generated content. Administrators upload media occasionally; Atlas stores
the original, generates a few fixed image renditions, and mostly delivers files
to visitors. Each site runs one Atlas instance on the same host as its
consuming application.

Consuming applications own their structured content and databases. Atlas owns
media files and media metadata only.

## Architecture and product decisions

- Rust with Axum; one service process per site.
- SQLite metadata and local persistent file storage on the same host.
- Nix flakes pin the development environment. Production hosts are ordinary
  Linux systems; NixOS is not required.
- Writes and metadata lookup require a Bearer API token used by a backend or
  CLI. Public original and rendition reads are unauthenticated.
- No browser-facing Atlas UI or direct browser uploads in the prototype. A
  future UI must keep the permanent token server-side and can use its backend
  to relay uploads or request short-lived upload grants.
- Preserve originals. Accept only web-ready image formats for processing, and
  those are exactly the formats Atlas itself produces, so a rendition can be
  uploaded again; reject camera RAW and other unconfigured formats even if
  ImageMagick can read them. Accept PDF, legacy Word, and DOCX as unconverted
  originals. `GET /api/v1/formats` reports the list from the same table the
  upload path validates against, so it cannot drift from it.
- Generate fixed maximum-width renditions, defaulting to 320, 640, 960, and
  1280 pixels. Preserve aspect ratio, never upscale, and report actual output
  dimensions. The consuming site decides the image's role and supplies its own
  `srcset`/`sizes` layout hints. Server-side crop/art-direction renditions are
  deferred.
- A `/w{max_width}` URL requests an image *no wider* than that width, not a
  specific file. Atlas serves the stored rendition for that profile when one
  exists, otherwise the widest stored rendition no wider than the request. A
  photo narrower than the largest profile therefore answers every profile with
  its own size instead of a blank response, nothing is ever upscaled, and a
  request narrower than anything stored is a distinct `rendition_not_found`,
  never a generic `asset_not_found`. Consumers should describe each stored
  rendition with its actual output width, which metadata reports separately
  from the requested profile.
- Each rendition records the name of the file that holds its bytes, because the
  file is named after the actual output width rather than the profile.
- At each width, generate JXL, AVIF, WebP, and one conventional fallback
  (JPEG for JPEG or opaque WebP sources; PNG for PNG or transparent WebP
  sources). Width profiles larger than the source do not upscale; duplicate
  actual widths are generated only once, and the skipped profiles resolve to the
  stored rendition on read. Public rendition URLs negotiate among
  pre-generated representations from `Accept`; they do not perform image work
  on reads and send `Vary: Accept`.
- Asset URLs are immutable. Replacing bytes means uploading a new asset ID.
- Identical bytes are the same asset. An upload whose original matches a live
  asset returns that asset with `200` instead of `201`, costs a hash rather than
  a conversion, and makes a retry after a timeout idempotent. Deduplication is
  best effort: enforcing it with a unique index would require deleting rows a
  live consumer may already reference.
- An optional `X-Original-Filename` header keeps the name the uploader gave the
  file, raw UTF-8 or percent-encoded. It is sanitized, truncated at 255
  characters, and used only in `Content-Disposition` — never as a path, and
  never to decide a stored format.
- Asset IDs are random lowercase base36 strings, 18 characters by default, from
  the OS random source. The ID is the only identifier: SQLite primary key,
  on-disk directory name, and public URL component. There is no separate
  internal or external identity, and no lookup column. ~93 bits of entropy puts
  50% collision odds around 10^14 assets; a lost race surfaces as a primary
  key violation and is retried with a fresh ID, bounded at five attempts.
  `ATLAS_ID_LENGTH` may change the length (12–24) without a migration, since
  existing rows keep the IDs they were issued.
- Files live in a private persistent data directory outside any public web
  root. The directory must be private to the Atlas service account.
- Deletion is a lifecycle, not an erasure. `DELETE` moves the bytes to a trash
  directory and stamps `deleted_at`; purge discards them after a retention
  window and leaves a tombstone. Public URLs keep working throughout and serve
  a short-cached placeholder while the asset is retired, so a page already
  referencing an asset never breaks. A consuming application can pin an asset
  it still uses, and that pin is the only hint Atlas ever gets about consumer
  content.
- Schema changes are additive SQL migrations applied at startup. Rebuilding a
  table is a last resort guarded against `ON DELETE CASCADE`, and startup
  refuses to serve from a database that fails `integrity_check` or
  `foreign_key_check`.

## What Atlas owns, and what it does not

Consumers share the logic of any API: errors, retries, transport. That is
expected. What Atlas absorbs is **common interaction logic for media
manipulation** — the capability every consumer would otherwise write from
scratch — while the burden of *interpreting meaning* stays with the consumer.
The working test: would every consumer write this, or only one?

Atlas owns: listing, filtering, sorting, paging and counts; bulk actions with
the trash lifecycle attached; rendition construction; deduplication signalling;
lifecycle state; the original filename; the pin. Consumers own: captions, alt
text, tags, folders, collections, ratings, post relationships, cover choice and
ordering — everything that gives media meaning within a site.

A pin is a boolean safety layer, not a usage graph. Atlas cannot know whether
media is in use without knowing the contents of the consumer, so a consumer
registers what it needs and interprets what `pinned` means for itself.

Folders and tags are Atlas's to store (above), deliberately *rudimentary*: flat,
opaque strings with no hierarchy, no colours and no ordering, so that every
project does not rebuild label CRUD, facet counts, filtering and rename/merge.
A consumer that wants nesting stores `2026/summer` as one label and does its
own splitting. The burden of interpreting every label stays with the consumer.
Hierarchy inside Atlas is refused on the record: it is how a media service
becomes a document management system.

### Kind is where the caller chose; format is what the bytes are

Two collections, `images` and `documents`, in the API. They differ in what they
offer, not in how they are stored:

- **Kind** is the caller's choice, expressed in the URL path. It decides which
  *operations* exist, whether renditions are generated, and whether the file is
  served as an attachment. It is a property of the call site, so a client that
  asks for a rendition is asking for an image by construction, and the server
  *enforces* the classification instead of trusting a header.
- **Format** is the stored bytes. It decides byte limits, magic-byte
  validation, whether ImageMagick may decode the file at all, and which
  conventional fallback a rendition uses.

The two are orthogonal: `documents` accepts `image/*` for the case where an
image should be stored and served untouched — no conversion, no renditions, an
attachment — which is the same reason a messenger offers to send a photo as a
document. Consequences that follow from keeping them apart: the format layer
still branches exactly where it does today; and deduplication must be keyed on
`(sha256, kind)`, because otherwise the first upload of some bytes would decide
every later one — a document first would answer a photo request with an asset
that has no renditions.

### Typed where mistakes cost something, uniform where mixing is natural

Mutations and rendition requests are typed: `POST /api/v1/images`,
`GET|DELETE /api/v1/{kind}/{id}`, `POST /api/v1/{kind}/{id}/restore`,
`PUT /api/v1/{kind}/{id}/pin`, `POST /api/v1/{kind}/actions`, and
`GET /api/v1/images/{id}/srcset`, which simply has no documents equivalent.
Read-only queries a consumer makes over a mixed set stay uniform:
`POST /api/v1/assets/state` for batch drift repair, `GET /api/v1/trash`,
`GET /api/v1/summary`. Public media URLs are untyped and immutable:
`/media/{id}/original` and `/media/{id}/w{width}`.

Handlers are shared behind one `Kind` value, so the split costs an enum and a
route rather than a second implementation of the lifecycle.

### If a third type arrives

A type that brings only different *storage* joins the existing collections as a
kind or format value. A type that brings new *operations* — transcoding status,
a poster frame, a re-encode — gets its own collection, because operations are
exactly what must be routed rather than guarded by a runtime check. Video and
audio are deferred, not refused.

## Configuration

Settings resolve in order: built-in defaults, then an optional TOML file named
by `ATLAS_CONFIG`, then environment variables. The TOML file holds non-secret
deployment shape and is safe to commit and share across sites; environment
variables override it for per-host differences. `ATLAS_API_TOKEN` is
deliberately excluded from the file schema so a committed config can never
carry a secret — unknown keys are rejected outright, which also surfaces typos.

## Implemented prototype API

Asset routes are typed by collection, `images` or `documents`, so a wrong
collection is a `404` rather than a silent no-op. Only the two mixed-set
endpoints (`state` and `trash`) are untyped, because they exist to span
collections.

- `GET /healthz` — process health.
- `POST /api/v1/images` — authenticated raw-body image upload. `Content-Type`
  must be an allowlisted image type and `X-Original-Filename` optionally names
  the file; the request body is streamed to a staging file with limits. Returns
  `201` after creating an asset, or `200` with the existing asset and
  `deduplicated: true` when those bytes are already live in this collection.
- `POST /api/v1/documents` — the same upload for documents. `Content-Type` may
  be any allowlisted format, images included; the file is stored and served
  untouched, with no renditions.
- `GET /api/v1/{kind}` — authenticated listing of that collection with `limit`,
  `offset`, `total`, `state` (`active`, `deleted`, `purged` or `all`), a `q`
  filename substring, a `mime` filter taking an exact media type or a group
  such as `image/*`, the `created_after`/`created_before`, `bytes_min`/
  `bytes_max` and `width_min`/`height_min` ranges, `sort` by `created_at`,
  `bytes`, `width` or `filename`, and `order`. A `mime` value that Atlas does
  not accept, or that the collection could never hold, is `400 invalid_mime`
  rather than an empty page: a filter that quietly matches nothing is worse
  than one that says why.
- `GET /api/v1/{kind}/{id}` — authenticated asset metadata, including its `kind`,
  original facts, available width renditions, and lifecycle state.
- `POST /api/v1/{kind}/actions` — authenticated bulk `pin`, `unpin`, `delete`,
  or `restore` for up to 500 caller-supplied ids, validated all-or-nothing.
- `GET /api/v1/summary` — authenticated counts and stored bytes, trash included.
- `GET /api/v1/formats` — authenticated accepted media types per collection,
  the `mime` groups they answer to, and whether renditions are generated.
- `POST /api/v1/assets/state` — authenticated batch state for up to 500 ids the
  caller supplies, across collections. Not a listing, and Atlas learns nothing
  from the ids.
- `DELETE /api/v1/{kind}/{id}` — authenticated move to the trash.
- `POST /api/v1/{kind}/{id}/restore` — authenticated restore from the trash.
- `PUT /api/v1/{kind}/{id}/pin` — authenticated usage registration.
- `GET /api/v1/trash` — authenticated listing of trashed and purged assets from
  both collections, paged with `limit` (default 200, max 500) and `offset`, with
  a `total`.
- `GET /api/v1/images/{id}/srcset` — authenticated `srcset` for one image, one
  candidate per stored width, described by the width actually produced.
- `GET /media/{id}/original` — public, unmodified original.
- `GET /media/{id}/w{max_width}` — public rendition with format selected from
  `Accept`; JXL, AVIF, WebP, then an acceptable JPEG/PNG fallback.

Public files support `ETag`, conditional requests, single byte ranges, a
one-year immutable cache policy, and `X-Content-Type-Options: nosniff`. PDFs are
served inline; Word files are served as attachments. Media paths use only
opaque lowercase base36 IDs and a configured width profile, never filesystem
paths or user-controlled names. Anything that fails ID validation resolves to
`404`.

A retired asset — trashed or purged — is still a known ID, so its URLs resolve
to an embedded placeholder with `Cache-Control: public, max-age=60` rather than
`404`; an unknown ID stays `404`. The placeholder is deliberately not immutable,
because a restore can bring the asset back.

This prototype does not yet provide search, an OpenAPI document, multipart
uploads, or arbitrary URL transformations.

## Upload validation and processing limits

- Stream request bodies to disk; never buffer an entire upload in memory.
- Defaults: 20 MiB per image, 50 MiB per document, 40 megapixels, and 12,000
  pixels maximum on either image edge. Configure byte/pixel limits with
  environment variables.
- Verify file signatures against the declared type. Signature mismatches,
  unsupported formats, empty files, byte overages, and decoded-dimension
  overages return clear errors.
- Invoke ImageMagick with argument lists, generated temporary paths, explicit
  formats, and per-process memory/map/disk/thread/time limits. Startup verifies
  read/write support for JPEG, PNG, WebP, AVIF, and JXL.
- Bound uploads to two concurrent requests, image conversions to one at a time,
  each ImageMagick invocation to 45 seconds, and all processing for one image
  to 120 seconds. No persistent job queue is needed for occasional uploads.
- Originals and generated renditions are staged together, then atomically
  promoted before metadata is committed. Startup removes incomplete staging
  data and unindexed asset directories after a crash.
- Trashed assets are purged at startup and, at most once an hour, after an
  upload. Purge records the tombstone before deleting bytes, so a crash leaves
  files that startup recovery sweeps rather than a row claiming bytes that are
  gone. Startup recovery returns interrupted deletes and restores to the state
  their rows record, and leaves the trash otherwise untouched.

HTTP errors use a small JSON envelope. Relevant statuses include `401`
unauthorized, `409` when a purged asset cannot be restored, `413` upload too
large, `415` unsupported/mismatched media type, `422` invalid or
unprocessable media, and `406` when no stored format matches the request's
`Accept` header. Two `404` codes separate "no such asset"
(`asset_not_found`) from "that asset holds no rendition this narrow"
(`rendition_not_found`, which lists the stored widths). Metadata that records a
file that is not on disk is a `500`, not a `404`: the request was valid and
storage is damaged.

## Example consumer

`examples/family-album/` is a small Flask application that consumes Atlas the way
a real site would. It owns a SQLite database of folders, descriptions, alt text,
tags, posts, and references; it holds the API token server-side; it deletes only
after checking its own posts for published references; and it uses the stored
rendition widths in `srcset`. It exists to prove the integration contract from
the consumer side and is not part of the service.


## Operations

- Development: run commands inside `nix develop`; lock the Nixpkgs revision in
  `flake.lock`.
- Production: bind Atlas to loopback behind the site's HTTPS reverse proxy;
  expose `/media/` publicly and keep token-authenticated API calls server-side.
- Keep the SQLite database and media directory persistent and on local storage.
- Snapshot the database with `VACUUM INTO`, never `cp` — WAL mode means a copied
  database file can be missing committed rows. Verify each snapshot, then copy
  the media directories. Test restoring both together.

## Verification focus

- Streaming limits hold with or without `Content-Length`.
- RAW/mismatched signatures and excessive decoded dimensions are rejected
  before publication, including between the formats that share an `ftyp` box:
  an AVIF renamed `.heic` is refused, and a bare JPEG XL codestream and a
  container JPEG XL are both recognised.
- Every format the renditions are written in is also accepted on upload, so a
  rendition fetched from Atlas can be uploaded again, and every format
  `GET /api/v1/formats` advertises is accepted by the endpoint that validates
  it.
- Every format ImageMagick writes a rendition in is also readable at startup,
  so a build missing a delegate refuses to start rather than failing on
  somebody's first upload.
- ImageMagick failures/timeouts and database failures do not publish partial
  assets; startup cleanup handles unindexed directories left by a crash.
- Auto-format negotiation honors `Accept` quality values, emits `Vary: Accept`,
  and uses a conventional fallback when appropriate.
- Public resolution rejects non-base36 or wrong-length IDs with `404` and cannot
  traverse outside the media root; validators and byte ranges refer to the
  selected stored representation.
- Config precedence is defaults < TOML file < environment; a config file
  containing a secret-like key is rejected; the API token is never read from a
  file.
- Deleting stops public delivery before the files move; a trashed asset serves
  the placeholder; restore returns the bytes to the same URLs; purge keeps a
  tombstone and skips pinned assets.
- An uploaded name survives non-ASCII, percent-encoding and path-like input,
  reaches only the `Content-Disposition` header, and cannot change the stored
  format or reach the filesystem.
- Identical bytes return the existing asset with `200`, write nothing, and
  leave no staged files behind.
- One state lookup covers a page, returns `missing` for unknown ids, refuses
  malformed ones, and is capped.
- Every filter on a listing is exercised, not just the unfiltered default. A
  clause that only applies under a filter can go wrong for as long as nobody
  uses that filter: the album's folder and tag views were broken for the life of
  the feature because nothing filtered the index, and a sibling query counting
  the same rows was missed with it.
- The trash listing is capped and pages without overlapping or skipping.
- A table rebuild guarded against `ON DELETE CASCADE` preserves child rows,
  and the same rebuild without the guard is caught by a test that proves the
  cascade would have deleted them silently.

## Deferred scope

- Admin UI and browser upload grants
- Rich tagging and folder hierarchy beyond the flat opaque labels above
- PDF/office rendering, OCR, and document conversion
- Video transcoding and thumbnail pipelines
- Remote object storage, multiple service instances, multi-tenancy, and
  distributed job queues
- An `animations` kind. Nothing multi-frame can be uploaded today, and the
  reason is structural rather than a missing check: `inspect` reads the first
  line of ImageMagick's per-frame report, and `generate` writes one file per
  `-write` target, so an animated input would be silently flattened *and* burn
  the conversion timeout. A kind that owns frames needs a rendition model that
  does too, not a special case in this one.
- Emitting the JPEG XL ISOBMFF container instead of the bare codestream
  ImageMagick writes. Progressive rendering does not depend on it — that is a
  codestream property — but the container carries Exif and XMP, and its `jxlp`
  boxes enable byte-range progressive delivery. Renditions are generated with
  `-strip` and so carry neither today. Worth researching whether ImageMagick's
  coder can be persuaded with a define, or whether that means a `cjxl`
  delegate.
- Wide-gamut colour handling. Renditions ask for sRGB before `-strip`, which
  leaves an untagged or already-sRGB source untouched. Whether that performs a
  true ICC transform for a Display P3 phone photo is **unverified**: this
  ImageMagick build ships no ICC profiles (`-list profile` is unsupported and
  no `.icc` is present), and no Display P3 file was available to test with.
  Confirm with a real photo before relying on it; the mechanism, if the flag
  is only tagging, is `-profile <sRGB target>` with profiles available.
- JPEG XL, AVIF and HEIC signatures are brand-checked rather than merely
  sniffed, because AVIF and HEIC are both ISO base media files starting with
  an `ftyp` box; the whole brand list is searched, since a file may declare
  `mif1` and list `heic` among its compatible brands.

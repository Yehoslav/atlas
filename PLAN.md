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
- Preserve originals. Accept only JPEG, PNG, and WebP for image processing;
  reject camera RAW and other unconfigured formats even if ImageMagick can read
  them. Accept PDF, legacy Word, and DOCX as unconverted originals.
- Generate fixed maximum-width renditions, defaulting to 320, 640, 960, and
  1280 pixels. Preserve aspect ratio, never upscale, and report actual output
  dimensions. The consuming site decides the image's role and supplies its own
  `srcset`/`sizes` layout hints. Server-side crop/art-direction renditions are
  deferred.
- At each width, generate JXL, AVIF, WebP, and one conventional fallback
  (JPEG for JPEG or opaque WebP sources; PNG for PNG or transparent WebP
  sources). Width profiles larger than the source do not upscale; duplicate
  actual widths are generated only once. Public rendition URLs negotiate among
  pre-generated representations from `Accept`; they do not perform image work
  on reads and send `Vary: Accept`.
- Asset URLs are immutable. Replacing bytes means uploading a new asset ID.
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

## Configuration

Settings resolve in order: built-in defaults, then an optional TOML file named
by `ATLAS_CONFIG`, then environment variables. The TOML file holds non-secret
deployment shape and is safe to commit and share across sites; environment
variables override it for per-host differences. `ATLAS_API_TOKEN` is
deliberately excluded from the file schema so a committed config can never
carry a secret — unknown keys are rejected outright, which also surfaces typos.

## Implemented prototype API

- `GET /healthz` — process health.
- `POST /api/v1/assets` — authenticated raw-body upload. `Content-Type` names
  the input type; the request body is streamed to a staging file with limits.
  Returns `201` only after persistence and image processing complete.
- `GET /api/v1/assets/{id}` — authenticated asset metadata, including original
  facts, available width renditions, and the asset's lifecycle state.
- `DELETE /api/v1/assets/{id}` — authenticated move to the trash.
- `POST /api/v1/assets/{id}/restore` — authenticated restore from the trash.
- `PUT /api/v1/assets/{id}/pin` — authenticated usage registration.
- `GET /api/v1/trash` — authenticated listing of trashed and purged assets.
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

This prototype does not yet provide asset listing, search, an OpenAPI
document, multipart uploads, or arbitrary URL transformations.

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
`Accept` header.

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
  before publication.
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
- A table rebuild guarded against `ON DELETE CASCADE` preserves child rows,
  and the same rebuild without the guard is caught by a test that proves the
  cascade would have deleted them silently.

## Deferred scope

- Admin UI, browser upload grants, asset listing/search, and tagging
- PDF/office rendering, OCR, and document conversion
- Video transcoding and thumbnail pipelines
- Remote object storage, multiple service instances, multi-tenancy, and
  distributed job queues

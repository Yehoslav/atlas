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
  facts and available width renditions.
- `GET /media/{id}/original` — public, unmodified original.
- `GET /media/{id}/w{max_width}` — public rendition with format selected from
  `Accept`; JXL, AVIF, WebP, then an acceptable JPEG/PNG fallback.

Public files support `ETag`, conditional requests, single byte ranges, a
one-year immutable cache policy, and `X-Content-Type-Options: nosniff`. PDFs are
served inline; Word files are served as attachments. Media paths use only
opaque lowercase base36 IDs and a configured width profile, never filesystem
paths or user-controlled names. Anything that fails ID validation resolves to
`404`.

This prototype does not yet provide asset listing, deletion, an OpenAPI
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

HTTP errors use a small JSON envelope. Relevant statuses include `401`
unauthorized, `413` upload too large, `415` unsupported/mismatched media type,
`422` invalid or unprocessable media, and `406` when no stored format matches
the request's `Accept` header.

## Operations

- Development: run commands inside `nix develop`; lock the Nixpkgs revision in
  `flake.lock`.
- Production: bind Atlas to loopback behind the site's HTTPS reverse proxy;
  expose `/media/` publicly and keep token-authenticated API calls server-side.
- Keep the SQLite database and media directory persistent and on local storage.
- Back up both metadata and files. Test restoring them together.

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

## Deferred scope

- Admin UI, browser upload grants, asset listing/search, and deletion workflows
- PDF/office rendering, OCR, and document conversion
- Video transcoding and thumbnail pipelines
- Remote object storage, multiple service instances, multi-tenancy, and
  distributed job queues

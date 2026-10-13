# Atlas project plan

## Goal

Build a small, robust, API-first media service for blog-like sites with little
user-generated content. Administrators upload media occasionally; the service
stores originals, creates a few useful image variants, and mostly serves files
to visitors. Each consuming site runs its own Atlas instance on the same host.

Consuming applications continue to own their structured content and their own
SQLite databases. Atlas owns media binaries and metadata about those binaries.

## Decisions and constraints

- **Language/API:** Rust with Axum. Keep the API strongly typed and the service
  a single deployable process.
- **Development tools:** Nix flakes provide the pinned Rust toolchain,
  ImageMagick, SQLite utilities, and other project tools. Production hosts are
  ordinary Linux systems; NixOS is not required.
- **Metadata/storage:** SQLite and a persistent local filesystem directory,
  both local to the host. One instance per consuming site; no multi-tenancy in
  the initial release.
- **Access:** Authenticated server-to-server/admin writes and public media
  reads. No browser-facing admin UI in the initial release.
- **Processing:** ImageMagick runs only during upload, as a bounded child
  process. Preserve originals and generate a small configured set of
  size-and-format variants: WebP, AVIF, and JPEG XL, with a conventional JPEG or
  PNG fallback. Store documents and other non-image files as originals; do not
  render or extract them initially.
- **Uploads:** Stream into a staging file while enforcing a configured byte
  limit. Initially accept web-ready JPEG, PNG, and WebP images for processing;
  do not accept camera RAW or other unconfigured formats even if ImageMagick can
  decode them. Enforce decoded width, height, and pixel limits before generating
  variants. Reject over-limit uploads clearly rather than attempting expensive
  conversions.
- **Starting image limits:** Evaluate defaults around 20 MiB per image and 40
  megapixels decoded, configurable per deployment. Select a separate byte cap
  for documents. These are initial values to validate, not immutable API
  guarantees.
- **Delivery:** Stable opaque asset identifiers and immutable variant URLs;
  public read responses should provide appropriate MIME types, cache headers,
  validators, and byte-range support where practical. Keep URLs
  format-specific, and have API responses expose available variant URLs so
  consuming templates can use `<picture>` with modern-format sources and a
  fallback. Do not vary bytes behind one URL based on `Accept` in the initial
  release.
- **Operations:** Keep service state in an explicitly configured persistent
  directory and document backup/restore for both the SQLite database and media
  files.

## Initial API shape

Exact route names and response schemas should be settled with the first API
implementation. The initial surface is expected to include:

- `GET /healthz` — process health for local monitoring.
- `POST /api/v1/assets` — authenticated upload, validation, metadata creation,
  and image variant generation.
- `GET /api/v1/assets/{id}` — authenticated or application-facing metadata
  lookup, finalized with the API contract.
- `DELETE /api/v1/assets/{id}` — authenticated deletion of metadata, original,
  and variants.
- `GET /media/{id}/{variant}` — public original/variant delivery.

Publish an OpenAPI description alongside the API. Use normal HTTP semantics:
for example, `413 Payload Too Large` for byte-limit violations and `415
Unsupported Media Type` for unsupported uploads.

## Implementation phases

### 1. Repository and development baseline

- Keep `flake.nix` and `flake.lock` as the reproducible development shell.
- Create the Rust crate, configuration loading, structured logging, and
  `/healthz` endpoint.
- Add formatting, lint, and test commands to the documented workflow.

### 2. Metadata and local storage

- Define the asset and variant metadata model and SQLite migrations.
- Create a configured data directory with safe permissions and opaque,
  server-generated storage keys.
- Implement staging and atomic promotion of completed uploads; avoid exposing
  partially written files.
- Define deletion and error-recovery behavior across metadata and files.

### 3. Authenticated upload and validation

- Add bearer-token authentication for write operations and an initial
  provisioning/rotation procedure for credentials.
- Stream request bodies to disk with enforced per-category byte limits; do not
  buffer whole uploads in memory.
- Validate actual file signatures and allowlisted input formats rather than
  trusting filenames or request MIME headers. The initial image allowlist is
  JPEG, PNG, and WebP; camera RAW formats are rejected and must be converted by
  administrators before upload.
- Add dimension/pixel checks and useful validation error responses.

### 4. Bounded image processing

- Invoke ImageMagick using safe argument passing and temporary files, never a
  shell-interpolated command.
- Configure ImageMagick's allowed coders and resource limits (memory, disk,
  dimensions, time, and threads) in addition to API-level checks.
- Bound conversion concurrency; no distributed or persistent job queue is
  needed for the expected upload frequency.
- Preserve the original and generate a small, fixed set of size variants in
  WebP, AVIF, and JPEG XL, plus a JPEG/PNG fallback appropriate to the source
  image. Confirm all four outputs work in the selected ImageMagick package and
  Nix development shell.
- Configure reasonable per-format quality and effort settings. In particular,
  keep AVIF/JXL encoding effort bounded because uploads are infrequent but run
  on small shared hosts.
- Leave PDFs, Word files, and videos unmodified; provide original delivery.

### 5. Public delivery and API contract

- Serve public assets by opaque ID, without exposing filesystem paths.
- Set correct content types, cache policy, validators, and appropriate
  `Content-Disposition`; support conditional requests and byte ranges where
  practical.
- Return format-specific URLs suitable for `<picture>` markup; preserve a
  broadly supported JPEG/PNG fallback and avoid requiring consumers to guess
  browser codec support.
- Publish OpenAPI and examples for upload, lookup, deletion, and public URLs.
- Ensure overwrites cannot silently change bytes behind an immutable URL.

### 6. Deployment and operations

- Provide a Linux service example (systemd), environment/configuration
  reference, persistent-directory layout, reverse-proxy guidance, and
  ImageMagick installation/policy requirements.
- Keep Atlas listening on loopback when deployed behind the site's public
  reverse proxy; expose public media reads and authenticated write routes as
  appropriate.
- Document backup and restore for metadata and files, including a tested
  recovery procedure.

## Verification focus

- Upload streaming enforces byte limits even without a valid `Content-Length`.
- Unsupported formats and oversized decoded dimensions are rejected before
  conversion/storage promotion.
- ImageMagick failures, timeouts, and resource-limit failures do not leave
  visible partial assets or orphaned metadata.
- Public delivery cannot escape the media root and returns the expected MIME
  type/cache behavior.
- Delete and backup/restore behavior keeps database metadata and stored files
  consistent.

## Deferred scope

- Admin UI and rich content-management features
- Video transcoding and thumbnail pipelines
- PDF/office document previews, OCR, and text extraction
- Direct browser uploads and signed upload URLs
- Shared multi-tenant instances, multiple service replicas, remote object
  storage, and distributed job queues

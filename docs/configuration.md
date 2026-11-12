# Configuration

Every setting Atlas reads: where the values come from, the `atlas.toml`
file, and the environment variables that override it. For how to run a
deployed instance, see [`deployment.md`](deployment.md).

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
conversion_concurrency = 1

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
| `ATLAS_CONVERSION_CONCURRENCY` | `1` | Concurrent image conversions (1–8) |
| `ATLAS_TRASH_RETENTION_DAYS` | 30 | Days a trashed asset and a finished re-render job are kept before purge |

Uploads are streamed to staging files; at most two uploads and the configured
number of conversions run concurrently. ImageMagick receives explicit
input/output formats and resource limits. Failed requests do not publish
partial assets. On startup,
Atlas applies any pending schema migrations, runs `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` (refusing to start if either reports a problem),
removes incomplete staging data, and returns any interrupted delete or restore
to the state its row records.

Schema changes live in `migrations/` and run through `sqlx::migrate!`. See
[`migrations/README.md`](../migrations/README.md) for the SQLite `ALTER TABLE`
limits, the `ON DELETE CASCADE` hazard when rebuilding a table, and the
additive-migration policy Atlas follows.

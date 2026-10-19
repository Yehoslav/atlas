# CMS Admin

A first-party **consumer** of Atlas, not a UI inside Atlas. Every media fact
this application shows comes over the public HTTP API, and its `Cargo.toml`
does not depend on the Atlas crate. That is deliberate: this is a controlled
real use case with which to test the API, and a useful demonstration that a
separate application can build a media library with Atlas.

## Run it

Start Atlas in one terminal, then run the dashboard in another. Both commands
run inside the repository's pinned environment:

```sh
nix develop
export ATLAS_API_TOKEN='the same token as Atlas'
export ATLAS_DATA_DIR="$PWD/data"
cargo run
```

Open <http://127.0.0.1:5001>. The dashboard requires one shared password and
keeps the Atlas token on the server:

```sh
export ATLAS_API_URL='http://127.0.0.1:3000'
export ATLAS_MEDIA_URL='http://127.0.0.1:3000' # browser-facing /media base; defaults to API URL
export ATLAS_API_TOKEN='the same token as Atlas'
export ADMIN_PASSWORD='a-long-random-password'
export ADMIN_LISTEN='127.0.0.1:5001'
cargo run --manifest-path examples/cms-admin/Cargo.toml
```

`ADMIN_COOKIE_SECURE=true` adds the `Secure` cookie attribute when HTTPS is
terminated by a reverse proxy. The default is suitable for local HTTP
development; bind to loopback and put a real deployment behind its TLS proxy.
Sessions are random, held in memory and expire after eight hours. Restarting
the dashboard invalidates them. This example has one shared password, not
user accounts or roles.

There is no local database. The dashboard does not yet have a use case for
Atlas's opaque `data` field and deliberately does not surface it. The page shows
what Atlas knows, and is explicit where Atlas cannot know what the consuming
site does with an asset.

## What it exercises

- `GET /api/v1/images` and `/documents`: search, media-type filters, lifecycle
  filters, sorting and paging.
- `GET /api/v1/formats`: the upload form takes its allowlist from Atlas rather
  than hardcoding an independently drifting list.
- `POST /api/v1/{kind}/actions`: bulk pin/unpin/delete/restore. The UI reports a
  rejected batch as rejected; it does not claim a partial success.
- `GET /api/v1/summary`: the sidebar totals.
- `GET /api/v1/trash`: the whole Atlas trash, including tombstones. Entries
  carry their `kind`, so restore uses the collection the asset actually lives
  in — a PNG stored as a document is not sent to the images route.
- `GET /api/v1/images/{id}/srcset`: the detail view copies the srcset Atlas
  computed.
- `PUT /api/v1/{kind}/{id}/pin`: the only usage signal Atlas acts on.
- The raw-body upload endpoints, with per-file outcomes and browser upload
  progress.

Atlas cannot answer "what uses this asset?" A CMS must consult its own content
database for that. This dashboard is stateless, so it does not pretend
otherwise: the detail view says so plainly. The family-album example shows the
other valid design, where a consumer owns references and can answer that
question from its own schema.

## Frontend and licences

The UI is server-rendered with MiniJinja and enhanced with vendored HTMX. A
small local JavaScript file adds upload progress and copy-to-clipboard; there
is no Node toolchain, CDN dependency, bundler or build step. CSS uses the
Catppuccin **Mocha** palette and a border-first, shadcn-inspired component
style — no Material UI and no component package.

The exact upstream versions and licences for HTMX and Catppuccin are recorded
in [`static/vendor/VENDORED.md`](static/vendor/VENDORED.md), with each licence
file alongside the vendored asset. The palette includes four upstream flavours
but only Mocha is used; there is no theme switch.

## Tests

```sh
cargo test --manifest-path examples/cms-admin/Cargo.toml
cargo clippy --manifest-path examples/cms-admin/Cargo.toml --all-targets --all-features -- -D warnings
```

The repository smoke test also starts the dashboard against the real Atlas
binary and walks upload → browse → pin → trash → restore:

```sh
cargo build
cargo build --manifest-path examples/cms-admin/Cargo.toml
nix develop --command python3 scripts/smoke.py
```

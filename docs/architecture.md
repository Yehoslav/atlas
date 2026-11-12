# Architecture

How Atlas is put together, and where everything in the repository lives. The
design record — the decisions and the reasoning behind them — is
[`PLAN.md`](../PLAN.md); this file describes what the decisions produced.

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
- **Immutable public URLs.** Replacing an original means uploading a new asset
  id, so a URL that is in a page cannot change meaning underneath it. A
  rendition URL names the *generation* it was handed out for, and re-encoding
  an asset writes a new generation beside the old one rather than over it.
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
| `tests/fixtures/` | A Display P3 profile, for the test that renditions convert colours rather than relabel them, and the reference photograph the fidelity test measures against. |
| `contrib/` | A systemd unit and an example environment file for a host install. |
| `Dockerfile` | The container image: the service and an ImageMagick with the codecs. |
| `PLAN.md` | The design record: the decisions, and the reasoning behind them. |
| `TODO.md` | What is open, and what has already been settled. |

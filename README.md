# Atlas

Atlas is an API-first media service for small sites. It stores media files,
generates a limited set of image variants, and delivers public assets. A site
uses its own Atlas instance alongside its application; the application keeps
its own structured content and database.

The service is designed for occasional, authenticated administrator uploads
and mostly public reads of blog media. It does not aim to be an exhaustive CMS.
For image uploads, administrators should provide web-ready JPEG, PNG, or WebP
files; camera RAW files are intentionally outside the initial upload scope.

## Planned stack

- Rust API using Axum
- SQLite for media metadata
- Persistent local filesystem storage
- ImageMagick for bounded, upload-time image conversion to WebP, AVIF, and
  JPEG XL, with conventional fallbacks
- Nix flakes for a reproducible development environment

See [PLAN.md](PLAN.md) for the initial scope, API outline, deployment model,
and implementation phases.

## Development environment

Install Nix with flakes enabled, then enter the development shell:

```sh
nix develop
```

The flake provides the Rust compiler and Cargo, rustfmt, Clippy, rust-analyzer,
SQLite tools, and ImageMagick. `flake.lock` pins the Nixpkgs revision so the
tooling is reproducible. Its ImageMagick package includes the JPEG, PNG, WebP,
AVIF, and JPEG XL codecs needed by the planned variants. Atlas will still
allowlist the formats it processes; codec availability does not mean every
ImageMagick-supported format is accepted by the API.

The initial repository scaffold does not yet contain the Rust application
crate. Once implementation begins, the usual checks will be available from the
development shell, including:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Initial deployment model

Production deployment is planned for ordinary Linux hosts rather than requiring
NixOS. Each site gets one Atlas process, a local SQLite database, and a
persistent media directory. The host must provide a compatible ImageMagick
installation with JPEG, PNG, WebP, AVIF, and JPEG XL codecs enabled. Public
media can be routed through the site's existing HTTPS reverse proxy; write
endpoints must remain authenticated. Consuming sites should use `<picture>`
sources for modern formats and include a JPEG/PNG fallback for browsers without
support for a particular format. On the host, `magick -list format` should
show read/write support for `JPEG`, `PNG`, `WEBP`, `AVIF`, and `JXL`.

## License

Atlas is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).

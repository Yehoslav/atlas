# Instructions for coding agents

## Project

Atlas is an API-first media service deployed alongside one consuming site on a
single Linux host. Read `README.md` and `PLAN.md` before making architectural
changes; update them when the planned scope or operational behavior changes.

## Development environment

- Use the pinned Nix flake: run commands inside `nix develop`.
- Keep compiler, formatter, linter, and external tool versions reproducible via
  `flake.lock`.
- Format Rust with `cargo fmt --check`; lint with `cargo clippy`; run relevant
  tests before considering implementation work complete.
- Keep dependencies focused. Prefer stable, maintained crates and explain new
  native/runtime dependencies in the README.

## Commits

- Prefix the subject with a type: `feat:`, `fix:`, `perf:`, `refactor:`,
  `docs:`, `test:`, or `chore:`.
- Write the subject in the imperative mood, about 60 characters including
  the prefix, with no trailing period.
- Follow the subject with a blank line and a body wrapped at 72 columns.
  Explain why the change was made and what it replaces; the diff already
  shows what changed.
- Add a `BREAKING CHANGE:` footer when callers must adapt. The HTTP API is
  Atlas's public contract.
- Name the `PLAN.md` or `TODO.md` item a change advances when that is not
  obvious from the subject.

## Product and architecture constraints

- Keep Atlas API-first; consuming applications own their structured content
  and databases. Atlas owns media files and media metadata only.
- The initial deployment is one Atlas instance per site, with local persistent
  storage and SQLite metadata. Do not add multi-tenant or distributed
  infrastructure without updating the plan first.
- Public media reads are unauthenticated; uploads and mutations require
  authentication. Never expose administrator credentials to browser code.
- Preserve originals and create only configured image variants. Documents and
  other non-image files are stored and delivered without conversion in the
  initial scope.
- Treat uploaded files as untrusted input, including administrator uploads.
  Enforce request byte limits while streaming, allowlist image formats, cap
  decoded dimensions/pixel counts, bound conversion time/resources/concurrency,
  and return clear client errors when a limit is reached.
- Do not accept camera RAW formats in the initial upload API; administrators
  should convert them to a web-ready image before upload, regardless of the
  codecs available in the ImageMagick build.
- Invoke ImageMagick with an argument list (never a shell command string),
  generated temporary paths, explicit formats/options, and bounded resources.
  Do not interpolate user-controlled names into commands or filesystem paths.
- Store files outside the public web root. Resolve public asset identifiers to
  server-controlled paths and prevent path traversal.
- Keep public asset URLs stable and immutable across replacement; set suitable
  cache validators/headers. Include file delivery and backup/restore behavior
  in relevant changes.

## Scope discipline

- Do not add an admin UI, video transcoding, document rendering/OCR, object
  storage, a persistent job queue, or multi-instance support unless requested.
- Update `PLAN.md` when a user-approved scope decision changes.
- New source files and substantial changes should include focused tests for
  meaningful behavior, especially upload limits, media validation, and storage
  path safety.

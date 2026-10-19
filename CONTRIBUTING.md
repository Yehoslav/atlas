# Contributing to Atlas

Atlas is an API-first media service. If you are here to *use* it, read
[`README.md`](README.md) first; this file is about changing it.

The design record is [`PLAN.md`](PLAN.md) — the decisions and the reasoning
behind them — and [`TODO.md`](TODO.md) — what is still open and what has
already been settled. Read the relevant part before changing anything
structural, and update them **with** the change, not after it. A commit that
changes how Atlas behaves without saying why is much less useful than the
code it changes.

[`AGENTS.md`](AGENTS.md) holds the working conventions: the commit format,
the product constraints, and the scope discipline.

## Getting the code

```sh
git clone https://github.com/<you>/atlas.git
cd atlas
```

## The toolchain

There are two ways to get a working environment. **Nix is the convenient one
and is not required**; if you would rather not install Nix, the plain
toolchain below works and is what the code actually needs.

### With Nix

```sh
nix develop
```

That gives you everything at pinned versions: Rust, Cargo, rustfmt, Clippy,
ImageMagick, SQLite, and Python with the example consumers' dependencies.
`flake.lock` pins the exact revisions.

### Without Nix

You need:

| Tool | Version | Notes |
| --- | --- | --- |
| Rust | 1.85 or newer | Edition 2024. `rustup component add rustfmt clippy` |
| A C compiler | any current | `cc`, `gcc` or `clang`. SQLite is compiled from source by `sqlx`, so no system SQLite is needed |
| ImageMagick | 7 | The `magick` command. Must read *and* write JPEG, PNG, WebP, AVIF, JPEG XL and HEIC — Atlas checks at startup and refuses to start without them. `magick -list format` shows what is there |
| Python | 3.11 or newer | For the smoke script and the examples. The Family Album itself works on 3.9 |

The ImageMagick requirement is the one that surprises people: a default
install on some systems lacks JPEG XL or HEIC, and Atlas will not start. If
`magick -list format` does not list them, your ImageMagick needs a different
build.

## Running it

```sh
cp .env.example .env
$EDITOR .env          # put a token in it
cargo run
```

Or export the variables yourself; a real environment variable always wins over
`.env`. The settings and their precedence are in the README's configuration
section.

## The checks

Run these before proposing a change. They are the same ones CI runs.

```sh
# The service
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

# The CMS Admin example (a separate crate, on purpose)
cargo fmt --check --manifest-path examples/cms-admin/Cargo.toml
cargo clippy --manifest-path examples/cms-admin/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path examples/cms-admin/Cargo.toml

# The Family Album example
cd examples/family-album
python -m pytest tests -q
ruff check .
ruff format --check .
ty check
cd ../..
```

And the end-to-end check, which starts the real binary and drives the real
examples over HTTP:

```sh
cargo build
cargo build --manifest-path examples/cms-admin/Cargo.toml
python scripts/smoke.py
```

`scripts/smoke.py` needs `requests` for the album's client check. It reports a
check it could not run as *skipped* rather than as a pass, so read the summary
line rather than the exit code alone.

## Making a change

- **Tests come with the change.** New behaviour needs a test that fails
  without it; a bug fix needs a test that failed before.
- **The examples are the integration test.** If a change to the API breaks a
  consumer, the smoke script should say so. The two examples are deliberately
  written as ordinary consumers — one Python, one Rust — so that a contract
  change shows up in both.
- **Fakes are not the server.** A test double is a model of the server, and a
  wrong model passes. When a doubt comes, check the real thing: the real
  binary, or a real HTTP call.
- **Keep the boundary.** Atlas owns media files and media metadata. Meaning —
  captions, alt text, tags, post relationships — belongs to the consumer. A
  change that moves meaning into Atlas is a change to the product, not an
  implementation detail, and it needs to be argued in `PLAN.md` first.
- **Additive migrations only.** See [`migrations/README.md`](migrations/README.md)
  for what SQLite can and cannot do, and for the two ways a table rebuild can
  destroy your data without saying so.

## Commits

One logical change per commit, in the imperative mood, with a body that
explains *why* rather than restating the diff:

```
fix: name the drafts that use a photo on its own page

The photo page said "No posts use this photo" while the posts page listed
that post with the photo attached. Both were reading the album's own
database; they disagreed because they ask the same question for different
reasons.
```

Prefix the subject with `feat:`, `fix:`, `perf:`, `refactor:`, `docs:`,
`test:` or `chore:`. Add a `BREAKING CHANGE:` footer when a caller has to
adapt — the HTTP API is Atlas's public contract.

## Reporting a problem

Open an issue. If it is a security problem, do not open an issue: follow
[`SECURITY.md`](SECURITY.md).

# Atlas

Atlas is an API-first media service for small sites. It stores media files,
generates a few fixed image renditions, and delivers public assets over HTTP.
Each site runs its own Atlas instance next to its own application; the
application keeps its own content and its own database.

*This project was written with the help of large language models. The design,
the code and the review are the author's.*

## Why Atlas exists

For a site built from the ground up, the media part is the same every time: an
upload endpoint that validates and limits the file, a handful of image sizes, a
way to find out whether last month's file is still there, and a trash that does
not break the pages already pointing at it. Off-the-shelf CMSs covered that
ground, but for the scope of these projects — usually student NGOs — they were
either more than the budget or more than the appetite. So the part that was
genuinely common got written down once, and the part that differs — content,
structure, meaning — stayed with each site.

## Why a service and not a library

A library would have to be compiled into the consuming application, which ties
every project to one language's ecosystem and one release cadence. Atlas is
reached over HTTP, so the consuming application can be written in anything: the
two examples in this repository are one Python and one Rust, and neither knows
anything about the other beyond the API. The cost is a process to run, a token
to manage and a database to back up; the gain is that the media limits, storage
rules and lifecycle are shared rather than reimplemented per project.

## How it is deployed

One Atlas instance per site, on the same host as the site, with its data
directory on local persistent storage. The API needs a bearer token, which
stays on the server or in a CLI and never reaches browser code; the media URLs
under `/media/` are public and served by a reverse proxy in front of both
processes. Metadata lives in SQLite next to the files, because the two are
useless separately.

## What it deliberately does not do, and why

| Not in Atlas | Why |
| --- | --- |
| No browser UI inside Atlas | The API token is permanent, and a browser-facing upload needs a grant flow. An operator interface belongs to the consuming site's own application — `examples/cms-admin/` is one, written as an ordinary consumer. |
| No camera RAW | RAW decoders are the least reliable corner of ImageMagick, and a half-decoded RAW is a permanently broken original. Convert first. |
| No upscaling; fixed width profiles | Resizing on demand is a CPU denial-of-service and an unbounded storage multiplier. Fixed profiles are predictable, cacheable and enumerable. |
| No arbitrary transforms, no crop, no art direction | Every crop depends on the consuming site's layout, which Atlas does not know. Consumers supply their own `sizes` and `srcset`. |
| No content search; no meaningful tags | Atlas knows the bytes, not what they mean. Captions, alt text and post relationships belong to the consumer. |
| No nested folders | A media service with a folder tree becomes a document management system. Flat opaque labels stop every project rebuilding label CRUD; the consumer keeps the hierarchy. |
| Deletion is a lifecycle, not an erasure | A published page must not break because someone deleted a file. Bytes move to a trash directory, are purged after a retention window, and a tombstone remains. |
| No video, no document rendering, no OCR | Different products. |
| No object storage, no multi-tenancy, no distributed job queues | One site, one host, occasional administrator uploads. Re-render jobs run in-process with their state in SQLite; a multi-instance scheduler would be a different design. |
| No animations yet | The rendition model is one frame per width. A multi-frame kind needs a multi-frame model. |

## Try it

```sh
nix develop
export ATLAS_API_TOKEN='a-long-random-secret'
cargo run
```

Or put the settings in a file, so they do not have to be exported every time:

```sh
cp .env.example .env
$EDITOR .env
cargo run
```

In another shell:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: image/png' \
  --data-binary @photo.png \
  http://127.0.0.1:3000/api/v1/images
```

The response carries the asset id and the rendition URLs; `GET /media/{id}/w640`
serves the picture. The full set of formats Atlas accepts is in
`GET /api/v1/formats`, and it is exactly the set Atlas produces, so anything it
serves can be uploaded again — the file you have just downloaded is not a dead
end.

## Documentation

Everything else lives in `docs/`, for a reader who goes looking for it:

- **[The HTTP API](docs/api.md)** — every route, how a rendition URL resolves,
  uploads and deduplication, and the trash lifecycle. The reference a consumer
  codes against.
- **[Architecture](docs/architecture.md)** — how the service is put together,
  and what lives where in the repository. The design record with the reasoning
  behind the decisions is [`PLAN.md`](PLAN.md), and [`TODO.md`](TODO.md) holds
  what is still open and what has already been settled.
- **[Deployment](docs/deployment.md)** — the systemd and container paths, what
  a host needs, the reverse proxy, upgrades, and backup and restore.
- **[Configuration](docs/configuration.md)** — every setting, its default, and
  the precedence between the TOML file, `.env` and the environment.

Two example consumers sit in `examples/`, deliberately written as ordinary API
clients rather than as part of the Atlas crate:

- [`examples/family-album/`](examples/family-album/) is a small Flask site that
  owns its own database of folders, captions and posts, and shows the
  integration contract from the consumer's side.
- [`examples/cms-admin/`](examples/cms-admin/) is a Rust operator interface: a
  password-protected media library that reads and writes over HTTP only.

To change Atlas itself, read [`CONTRIBUTING.md`](CONTRIBUTING.md) — the
toolchain, the checks, and the working conventions.

## Status

Atlas is a prototype, and it says so in its API. The HTTP contract is pre-1.0
and may change in ways that break a consumer; the schema only ever moves
forward, additively, but the routes and the responses are not frozen.

## License

Atlas is licensed under the GNU General Public License, version 3. See
[`LICENSE`](LICENSE).

The example consumers are part of this repository and carry the same licence.
The vendored front-end assets keep their own: HTMX is Zero-Clause BSD and the
Catppuccin palette is MIT, and both licence files sit beside the files
themselves. The two ICC profiles, `assets/sRGB-v4.icc` and
`tests/fixtures/DisplayP3-v4.icc`, are from
[Compact ICC Profiles](https://github.com/saucecontrol/Compact-ICC-Profiles)
and are released under CC0.

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
| No object storage, no multi-tenancy, no job queues | One site, one host, occasional administrator uploads. |
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

## Implemented prototype API

Every route below that reads, changes, or renders an asset is *typed*: the
collection is part of the path, `images` or `documents`. A mistake is a `404`
rather than a silent no-op, and a reader can see which collection a call acts
on. Only the two mixed-set endpoints (`state` and `trash`) are untyped, because
they exist precisely to span collections.

An asset's **kind** is what it is used for — `images` get renditions,
`documents` do not — while its **format** is its media type: a JPEG, a PNG, a
PDF. The two are independent. `POST /api/v1/documents` accepts any allowlisted
format, images included, so a diagram or a scan can be stored whole and served
as an attachment instead of being re-encoded into a picture.

- `GET /healthz` — local process health.
- `POST /api/v1/images` — authenticated image upload. The body is the file
  itself; `Content-Type` must be `image/jpeg`, `image/png`, `image/webp`,
  `image/avif`, `image/jxl` or `image/heic`, and
  the optional `X-Original-Filename` header names it. Streams to disk, generates
  the configured renditions, and returns an asset ID, original metadata, and
  rendition URLs.
  A HEIC photograph may also be declared as `image/heif`: freedesktop.org
  makes `image/heif` the canonical type for a `.heic` file and keeps
  `image/heic` as its alias, so a browser, a file manager and a command line
  disagree about which spelling to send. Either is accepted, the bytes decide
  what the file is, and the asset is stored and served as `image/heic`
  whichever spelling it arrived under. `image/heic-sequence` is not a picture
  and is not accepted.
- `POST /api/v1/documents` — the same upload for `documents`. `Content-Type` may
  be any allowlisted format, images included. The file is stored and served
  untouched, with no renditions.
- `GET /api/v1/{kind}` — authenticated listing of that collection, paged with
  `limit` (default 50, max 200) and `offset`, and returning `total`. Also
  accepts `state=active|deleted|purged|all` (default `active`), a `q` substring
  of the original filename, a `mime` filter, the ranges `created_after`,
  `created_before`, `bytes_min`, `bytes_max`, `width_min`, `height_min`, `sort`
  of `created_at`, `bytes`, `width` or `filename`, and `order=asc|desc`.
  `deleted` and `purged` are different answers: a deleted asset can still be
  restored, a purged one cannot. An unknown value is a `400 invalid_sort`/
  `invalid_state`/`invalid_mime` rather than a quietly ignored filter. This is
  a listing of assets the site uploaded; it says nothing about how the site
  uses them.

  `mime` takes an exact media type (`image/png`) or a whole group
  (`image/*`), and answers the question a collection cannot: `documents` holds
  PDFs, Word files and pictures stored whole, and `?mime=image/*` is what finds
  the last of those. A value Atlas does not accept, and a value this collection
  could never hold, are both `400 invalid_mime`; `GET /api/v1/formats` says
  which types are on offer.
- `GET /api/v1/{kind}/{id}` — authenticated metadata lookup, including the
  asset's `kind`, `state` (`active`, `deleted`, or `purged`), `pinned` flag,
  and the rendition `generation` its URLs name.
- `POST /api/v1/{kind}/actions` — authenticated bulk action for ids the caller
  already has, such as `pin`, `unpin`, `delete`, or `restore`. Body
  `{"action": "pin", "ids": ["…"]}`, max 500 ids. Validation is all-or-nothing:
  one unknown id, a duplicate, or an action the current state does not allow
  rejects the whole batch with `400` and changes nothing.
- `GET /api/v1/summary` — authenticated counts and stored bytes for the whole
  instance, including the trash, and the `rendition_widths` the instance
  generates. A cheap dashboard number, not a listing. The widths are there
  because the configuration is otherwise invisible: a consumer can only find
  them out by listing every asset and reading the renditions of each one,
  which is how a deployment with the wrong widths finds out that every one of
  its assets is missing a width its site needs.
- `GET /api/v1/formats` — authenticated list of what each collection accepts,
  the `mime` groups they answer to, and whether they get renditions. Derived
  from the same table the upload path validates against, so it cannot
  advertise something Atlas would then refuse. A consumer building an upload
  form should read this rather than hardcode a list that drifts.
  It answers two questions with two lists, because one list cannot answer both:
  `formats` holds one media type per format — the canonical spelling, the one
  the asset is stored and served as — so counting it counts formats, and
  `media_types` holds every spelling a caller may put in a `Content-Type`,
  which is more than one for a format with alternative names. A consumer
  offering a filter should offer one option per format, from `formats`; a
  consumer validating a type somebody else declared should read `media_types`.
- `POST /api/v1/assets/state` — authenticated batch state lookup for ids the
  caller already has, across both collections. Not a listing.
- `DELETE /api/v1/{kind}/{id}` — authenticated move to the trash. Returns `204`.
- `POST /api/v1/{kind}/{id}/restore` — authenticated restore from the trash.
  Returns `204`, or `409` if the asset was already purged.
- `PUT /api/v1/{kind}/{id}/pin` — authenticated usage registration. Body
  `{"pinned": true|false}`; purge skips pinned assets.
- `PUT /api/v1/{kind}/{id}/data` — authenticated consumer-owned note. Body
  `{"data": "..."}`, or `{"data": null}` to clear. Atlas stores it, caps it at
  `ATLAS_MAX_CONSUMER_DATA_BYTES` (64 KiB) and hands it back untouched: it does
  not read the value, validate its shape, search it or log it. Returned by
  `GET /api/v1/{kind}/{id}` and deliberately absent from listings, so a page of
  assets cannot grow into a page of blobs.
- `GET /api/v1/trash` — authenticated listing of trashed and purged assets from
  both collections, paged with `limit` (default 200, max 500) and `offset`.
- `GET /media/{id}/original` — public original delivery.
- `GET /media/{id}/w{width}` — public width rendition. Atlas selects a
  pre-generated JXL, AVIF, WebP, or JPEG/PNG fallback using `Accept` and returns
  `Vary: Accept`. The optional `g` parameter names the generation, and the URLs
  the API hands out always carry it: `?g=2` is immutable and cached for a
  year, while the bare URL serves the current generation and is cached for an
  hour. An unknown generation is a `404 generation_not_found`; a `g` that is
  not a positive integer is a `400 invalid_generation`. The optional `format` parameter names the representation
  instead: `?format=webp` serves the WebP whatever the `Accept` header says,
  and the response carries no `Vary: Accept`, because the URL already says
  which representation it is. An unknown format name is a `400
  invalid_format`; a format the asset does not have at that width is a `404`.
- `GET /api/v1/images/{id}/srcset` — authenticated ready-made `srcset` for one
  image, one candidate per distinct actual output width. Each URL is a profile
  path and each descriptor is the width that was really produced, so a photo
  narrower than its largest profile is described honestly without duplicate
  candidates. Without a `widths` parameter it describes the instance's
  configured widths, so it is the answer a consumer gets without knowing the
  configuration. Errors on an unknown asset (`404`), a non-image, or a width
  Atlas does not have. Its URLs name the current generation, which the
  response also reports as `generation`.
- `POST /api/v1/images/{id}/renditions` — authenticated generation of the
  renditions an asset is missing. The widths an asset has are fixed when it is
  uploaded, so a deployment that configures the wrong widths leaves assets
  that can never gain the widths its site needs, and nothing about that is
  recoverable from the outside: uploads are content-addressed, so the same
  bytes return the same asset, and uploading again under a new id leaves every
  reference a consumer holds pointing at a tombstone. This is the repair. The
  original bytes do not change, so the asset id does not have to change
  either, and a consumer's references keep working. It answers with the
  widths the instance generates, the widths this call added, and the asset's
  renditions as they are now. It never replaces a rendition that is already
  there: a public URL that changes its bytes is a lie to every cache that
  holds it.

  With the body `{"re_encode": true}` it instead re-encodes every rendition
  from the original as a new generation: every profile the asset has, plus
  any configured width it lacks. The old generation's files stay, so every
  URL that names it keeps its bytes; the asset id does not change; and the
  answer carries the new `generation`, the renditions and the `srcset`, so a
  consumer that stores a srcset learns the new URLs in the same call. This is
  the way out of an encoding that turned out to be bad. A trashed image is a
  `409`, and an unknown option in the body is a `400`.
- `POST /api/v1/images/re-render` — authenticated re-encode of every live
  image, one at a time and each under the conversion slot, a page per call.
  Body `{"limit": 10, "after": "…"}`, both optional (`limit` max 100). The
  answer has the `count` re-encoded, the `re_encoded` ids with their new
  generation, the `failures`, how many images are `remaining`, and `next`, the
  `after` for the following page, `null` once the walk is done. The cursor
  moves past a failure, so collect the `failures` as you go. Trashed images
  are skipped; re-encode one after restoring it.

Uploads are content-addressed within their collection. If the exact bytes are
already live there, nothing is written or converted and the response is `200`
with the existing asset and `"deduplicated": true`. A duplicate is not an
error, and the alternative is paying for a conversion and storing the result
twice.

The default rendition widths are 320, 640, 960, and 1280 pixels. They preserve
the original aspect ratio and are not upscaled. Configure a different fixed
set with `ATLAS_VARIANT_WIDTHS=320,640,1280`; arbitrary sizes are not generated
on public requests. `GET /api/v1/summary` reports the widths the instance
generates, so a consumer can check its assets against the configuration in one
request. `POST /api/v1/images/{id}/renditions` generates the ones an asset is
missing, which is how a deployment that configured the wrong widths repairs
its assets without re-uploading them. Consuming sites can fetch
`/api/v1/images/{id}/srcset` and set their own `sizes` value. Atlas does not
index media *content* — it filters on filenames, media types and lifecycle
state, and leaves meaning to the consumer — and it never converts or
re-encodes on a read.

## How a rendition URL resolves

`/media/{id}/w960` asks for an image **no wider than** 960 pixels, not for a
particular file. Atlas serves the rendition stored under the `960` profile when
there is one, and otherwise the widest stored rendition that is not wider than
the request.

That distinction matters for small photos. A 500-pixel photo asked for `w960`
has no 960-pixel rendition, because Atlas does not upscale, so it is answered
with its 500-pixel rendition rather than a blank response. Metadata reports the
two separately: `max_width` is the requested profile, `width` is what was
actually produced. Describe a stored rendition in `srcset` with `width` — the
real output width — while keeping the URL pointed at the profile:

```html
<!-- A 500-pixel photo stored under profiles 320, 640, 960 and 1280 -->
<img src="/media/4f2k9xq7m1b8w3z0pd/w960?g=1" srcset="/media/4f2k9xq7m1b8w3z0pd/w320?g=1 320w, /media/4f2k9xq7m1b8w3z0pd/w640?g=1 500w" sizes="640px" alt="…">
```

Asking for something narrower than anything stored is a `404` with
`rendition_not_found`, listing the widths that do exist. That is deliberately
different from `asset_not_found`, which means Atlas has never heard of the ID:

```sh
curl http://127.0.0.1:3000/media/4f2k9xq7m1b8w3z0pd/w64
{"error":{"code":"rendition_not_found","message":"The asset has no rendition at or below 64 pixels; stored widths are 320, 640, 960, 1280."}}
```

### Asking for a format by name

The width says how wide; the format says which representation. By default the
format is negotiated from the `Accept` header, which is right for a browser
and wrong for a crawler that has to fetch an image for a marketplace:
Google's Merchant Center accepts JPEG, PNG, GIF, BMP, TIFF and WebP, and a
modern Googlebot sends

```
Accept: image/avif,image/webp,image/apng,image/*,*/*;q=0.8
```

so the negotiation answers AVIF — the one format the marketplace does not
accept. The `format` parameter names the representation instead:

```sh
curl -o thumb.webp 'http://127.0.0.1:3000/media/4f2k9xq7m1b8w3z0pd/w800?format=webp'
```

The parameter wins over the header, and the header is not consulted at all. The
response carries no `Vary: Accept`, because the URL already says which
representation it is and one copy per URL is enough. The names are the ones
Atlas uses in a response: `jxl`, `avif`, `webp`, `jpeg`, `png`.

Nothing is created, upscaled or stored: every width already has a JXL, an
AVIF, a WebP and a conventional fallback, and the parameter only chooses
among them. A format the asset does not have at that width is a `404`, not a
fallback to the negotiation — a caller that names a format and gets a
different one has been handed a picture it cannot use, in a response that
says it succeeded.

### Generations

A rendition's bytes are fixed when they are written, and an encoding that
turns out to be bad would otherwise be permanent. So the renditions carry a
**generation**, which starts at 1, and the URLs the API hands out name it:
`/media/{id}/w800?g=2`. Re-encoding an asset writes generation 3 beside it,
from the original, and moves the asset to it.

The promise is that a URL the API hands out is immutable: `?g=2` serves the
same bytes for as long as the asset lives, and is cached for a year. The one
generation URL that is not is a profile the current generation does not have
yet, which falls back to a narrower rendition until the profile is added; the
API never hands such a URL out, and it is served with the one-hour policy.
The bare `/media/{id}/w800` serves whichever generation is current and is
cached for an hour, so a page that still uses it picks up a re-encode soon
after it happens. The asset id never changes; a consumer that stores a srcset
refreshes it from the re-encode's answer or from
`GET /api/v1/images/{id}/srcset`.

```sh
curl -X POST -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/json' -d '{"re_encode": true}' \
  http://127.0.0.1:3000/api/v1/images/4f2k9xq7m1b8w3z0pd/renditions
```

After an encoder change — a newer ImageMagick, a corrected flag — every image
needs it. `POST /api/v1/images/re-render` walks the live images a page per
call; pass `next` back as `after` until it is `null`. The cursor moves past an
image that failed, so `null` means every live image was tried, not that every
one was re-encoded. Keep the `failures` of each page: every entry names its
image, and once the cause is fixed each one is re-encoded on its own with
`{"re_encode": true}`.

A re-encode is not free and not idempotent. Every one writes a new copy of
the asset's renditions and keeps every older copy until the asset is purged,
so the disk an image's renditions take grows by one copy per re-encode, and
the consumer's stored srcsets change each time. A page retried from the same
`after` re-encodes the images it already did; keep pages small and resume from
the last `next` that came back.

A page re-encodes its images one after another inside the request, so it holds
the request open for one conversion per image. Behind a proxy, keep `limit`
small enough that a page finishes inside the proxy's read timeout (nginx waits
60 seconds by default): a page the proxy gives up on may already have
re-encoded some of its images, and retrying it re-encodes those again.

## Asset IDs

Asset IDs are random lowercase base36 strings, 18 characters by default
(`4f2k9xq7m1b8w3z0pd`). They carry about 93 bits of entropy, drawn from the
operating system's random source. With that entropy the odds of a collision
remain negligible until roughly 10¹⁴ assets, far beyond one site's needs.

IDs are opaque and generated by the server. Do not parse them, derive meaning
from them, or treat them as a secret — public media is public by design.
Because they are lowercase, they survive mail clients and proxies that
normalize path case; only rely on standard, case-preserving path routing.

`ATLAS_ID_LENGTH` changes the length between 12 and 24 characters. Existing
assets keep their original IDs, so the setting can be changed without a
migration.

Use a width rendition for displayed images; the original URL always serves the
uploaded format. For example:

```html
<img
  src="/media/4f2k9xq7m1b8w3z0pd/w640"
  srcset="/media/4f2k9xq7m1b8w3z0pd/w320 320w, /media/4f2k9xq7m1b8w3z0pd/w640 640w"
  sizes="(max-width: 640px) 100vw, 640px"
  width="640"
  height="853"
  alt="Description owned by the consuming site"
/>
```

Uploads require `Authorization: Bearer $ATLAS_API_TOKEN`. Keep this token in the
consuming server or CLI, never in browser code. Public media reads do not need a
token.

## Uploading a file

The body is the file. The collection in the path decides what happens to it,
`Content-Type` names the allowed format, and the optional
`X-Original-Filename` header names the file for downloads:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/pdf' \
  -H 'X-Original-Filename: menu-2026.pdf' \
  --data-binary @menu-2026.pdf \
  http://127.0.0.1:3000/api/v1/documents
```

A picture goes to `/api/v1/images`, which generates the configured renditions:

```sh
curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: image/png' \
  -H 'X-Original-Filename: diagram.png' \
  --data-binary @diagram.png \
  http://127.0.0.1:3000/api/v1/images
```

That same diagram to `/api/v1/documents` instead is stored exactly as uploaded,
generates no renditions, and is served as an attachment. Both are legitimate;
choose the collection by whether the picture needs to be *displayed* or
*preserved*.

`201` means a new asset was created. `200` means an asset with the same bytes
already existed in that collection and this is that asset — identical content is
the same asset, because URLs are immutable and content is what identifies one. A
duplicate costs a hash rather than an image conversion, and a retry after a
timeout gets the same asset instead of a second copy.

The name is optional metadata and is never used as a path: the file on disk is
named after the asset ID and the detected format. It arrives as raw UTF-8 or
percent-encoded, so `Отчет за 2026.pdf`, `Jahresbericht (Ü).pdf` and
`100% pure.pdf` all survive. Names are truncated at 255 characters, any path is
stripped, and a name containing control characters is refused with
`400 invalid_filename`. Delivery sends it as `Content-Disposition: inline;
filename="…"; filename*=UTF-8''…`, falling back to `asset.pdf` when the upload
had no name. Consumers that want a different display name should keep it in
their own database.

## Asking what Atlas can still serve

A consumer that owns its own database still has to ask Atlas whether the assets
it points at are still there, because Atlas forgets files without telling
anyone. Asking once per asset is what makes a gallery page expensive, so:

```sh
curl -X POST http://127.0.0.1:3000/api/v1/assets/state \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"ids":["8odfjrxxjxzypa99j6","aaaaaaaaaaaaaaaaaaaa"]}'
```

```json
{"assets":[
  {"id":"8odfjrxxjxzypa99j6","state":"active","pinned":true},
  {"id":"aaaaaaaaaaaaaaaaaaaa","state":"missing","pinned":false}
]}
```

One call covers up to 500 ids. Ids Atlas has never heard of come back as
`missing` rather than being absent, because telling "never existed" apart from
"deleted" is the whole reason to ask. A malformed id is a `400`, not a silent
omission. This is not a listing: the caller supplies the ids, so Atlas learns
nothing about what they are used for and the result cannot be paged through.

## Trash, restore, and purge

Atlas never destroys an asset on request. `DELETE /api/v1/images/{id}` moves
`assets/<id>` to `trash/<id>` and stamps `deleted_at`. The soft delete is
recorded *before* the files move, so from the first moment of the delete the
public URLs stop serving the real bytes and answer with a small embedded
placeholder image (`assets/placeholder.png`, compiled into the binary) sent with
`Cache-Control: public, max-age=60`. That is deliberate: the placeholder is not
immutable, because a restore can bring the asset back, and an immutable
placeholder could outlive the restore by a year.

Once an asset has been in the trash for `ATLAS_TRASH_RETENTION_DAYS` (30 by
default) and no consumer has pinned it, purge discards the files and keeps the
row as a tombstone with `purged_at`. Atlas can then still tell "deleted" from
"never existed": an unknown ID is `404`, a tombstoned ID serves the placeholder,
and `GET /api/v1/images/{id}` reports `state: "purged"`. Rendition rows go with
the bytes, so a tombstone does not advertise URLs that would resolve to
nothing. Purge runs at startup and, at most once an hour, after an upload;
there is no scheduler and no job queue.

Pins are the one hint Atlas has about consumer content. Register an asset that
a published post still uses and Atlas will never purge it.

Nothing here is a replacement for a consuming site's own referential checks —
Atlas cannot know which of its assets your posts mention.

### If a consumer and Atlas have drifted

Atlas cannot repair itself: it has no listing, and it is not told when the
media directory and the database come back from different backups. The repair
is on the consuming side, and it is bounded:

1. Collect the asset IDs your database references.
2. Ask `POST /api/v1/assets/state` for them in batches.
3. Every `missing` ID is an asset that no longer exists and cannot be
   recovered; re-upload the file and repoint your references to the new ID.
4. Every `deleted` or `purged` ID is recoverable only while the bytes are in the
   trash; `POST /api/v1/images/{id}/restore` brings them back.

A media directory restored on its own will report every asset as `missing`,
which is the signal to stop serving those URLs rather than a silent failure.

## Development

Install Nix with flakes enabled, then enter the pinned development environment:

```sh
nix develop
```

The shell provides Rust, Cargo, rustfmt, Clippy, rust-analyzer, SQLite,
ImageMagick, and build tools. `flake.lock` pins Nixpkgs. On startup, Atlas checks
that ImageMagick can read/write JPEG, PNG, WebP, AVIF, JPEG XL and HEIC, and
that it is built with LCMS, which converts a photo's colour profile to sRGB, and
refuses to start without them. The API still accepts only its explicit input
allowlist.

The dev shell makes the same codec check as it opens, and CI runs it as a step
of its own (`scripts/check-magick-codecs.sh`), so a Nixpkgs bump that dropped
libheif is a named failure rather than a server that will not start or a phone
photograph that will not upload. The check reads ImageMagick's codec listing
in both of the shapes it is printed in — with and without loadable modules,
which is how Debian and Ubuntu ship it — so a host that has every codec is not
told it has none.

Run locally:

```sh
export ATLAS_API_TOKEN='replace-with-a-long-random-secret'
export ATLAS_DATA_DIR="$PWD/data"
cargo run
```

Or put those in a file instead, so they do not have to be exported every
time:

```sh
cp .env.example .env
$EDITOR .env
cargo run
```

> **Migrating from the UUID prototype:** asset IDs changed from UUIDs to short
> base36 strings. Assets created by an earlier prototype build keep their UUID
> in the database and become unreachable. Delete the development data directory
> and start fresh: `rm -rf ./data`

The default listener is `127.0.0.1:3000`. Example PDF upload from another
terminal:

```sh
export ATLAS_API_TOKEN='same-secret-as-the-running-server'

curl -X POST \
  -H "Authorization: Bearer $ATLAS_API_TOKEN" \
  -H 'Content-Type: application/pdf' \
  --data-binary @report.pdf \
  http://127.0.0.1:3000/api/v1/documents
```

Useful checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

`cargo test` runs the real ImageMagick: one test converts
`tests/fixtures/reference.jpg` and measures how far each rendition is from
the source, in decibels, with a floor per format. A change to the encoder
settings that quietly degrades every image the service has ever produced
passes every other test in the suite — they check that a rendition exists,
that it has the right width, that it has the right signature, and none of
them can see that the picture got worse. Run the tests inside `nix develop`,
where the ImageMagick is the one the floors were measured against.

### End-to-end check against a real binary

The test suite drives the router in process, and the example's tests replace
Atlas with an in-memory fake. Neither can notice when the two disagree about the
API, which is a real risk when one side is written by the same hand as the other.
`scripts/smoke.py` starts the built binary on a throwaway data directory, checks
the HTTP contract over a real socket with real conversions, points the example
consumers' own clients at it, and always stops the servers afterwards:

```sh
cargo build
cargo build --manifest-path examples/cms-admin/Cargo.toml
nix develop --command python3 scripts/smoke.py
```

It exits non-zero on the first failed expectation, prints which checks were
skipped rather than reporting them as passes, and needs no arguments: the port,
binary, and data directory all have defaults that `--port`, `--bin` and `--data`
can override. It checks the HTTP contract, the album's real Python client, and
— when its binary exists — the CMS admin UI against the same real Atlas.

## Example consumer

[`examples/family-album/`](examples/family-album/) is a small Flask site that
consumes Atlas the way a real one would: it owns its own database of folders,
descriptions, alt text, tags, and posts, keeps the API token on the server,
checks its own posts before deleting, and renders the stored rendition widths
in `srcset`. Its own tests run against a fake Atlas:

```sh
cd examples/family-album
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements-dev.txt
python -m pytest tests -q
ruff check .
ruff format --check .
ty check
```

`requirements-dev.txt` adds the standalone test and lint tools. The repository's
Nix development shell also provides Python, Flask, requests, pytest, ruff and
ty at pinned versions, so `nix develop` is the reproducible path.

### CMS Admin

[`examples/cms-admin/`](examples/cms-admin/) is a second first-party consumer,
written in Rust, that exercises the same API from a different kind of site. It
is a standalone crate and does not depend on the Atlas crate: all reads and
mutations go through HTTP. It provides a password-protected media library with
search, filters, bulk actions, upload progress, asset detail, and the global
trash. Its Catppuccin Mocha / HTMX frontend has no Node build step; vendored
assets and their licences are recorded in the example.

```sh
nix develop
export ATLAS_API_URL='http://127.0.0.1:3000'
export ATLAS_API_TOKEN='the-same-long-random-secret-as-Atlas'
export ADMIN_PASSWORD='a-long-random-password'
cargo run --manifest-path examples/cms-admin/Cargo.toml
```

The dashboard is stateless. It deliberately does not claim to know what uses
an asset: only the consuming site's own content database can answer that.

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
| `ATLAS_TRASH_RETENTION_DAYS` | 30 | Days a trashed asset is kept before purge |

Uploads are streamed to staging files; at most two uploads and one conversion
run concurrently. ImageMagick receives explicit input/output formats and
resource limits. Failed requests do not publish partial assets. On startup,
Atlas applies any pending schema migrations, runs `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` (refusing to start if either reports a problem),
removes incomplete staging data, and returns any interrupted delete or restore
to the state its row records.

Schema changes live in `migrations/` and run through `sqlx::migrate!`. See
[`migrations/README.md`](migrations/README.md) for the SQLite `ALTER TABLE`
limits, the `ON DELETE CASCADE` hazard when rebuilding a table, and the
additive-migration policy Atlas follows.

## Deploying Atlas

Production targets ordinary Linux hosts, not specifically NixOS. Atlas is one
process next to the site it serves, behind that site's reverse proxy, with a
private directory of its own.

There are two ways to deploy it: a systemd unit on a host, or a container
image. Both need the same three things — the binary, an ImageMagick with the
codecs, and a private data directory — and both put the token in a file
rather than on a command line.

### What a host needs

- **The binary.** `cargo build --release` produces `target/release/atlas`. It
  links SQLite statically, so there is no SQLite library to install.
- **ImageMagick with the codecs.** Atlas reads and writes JPEG, PNG, WebP,
  AVIF, JPEG XL and HEIC, and converts colour profiles through LCMS. It
  refuses to start without them, which is the right refusal: a build without
  them fails on somebody's first upload, or on a phone photograph, long
  after the cause. Check a host before installing:

  ```sh
  bash scripts/check-magick-codecs.sh
  ```

  This is the check the development shell makes as it opens, and the one the
  container image makes as it builds. It names the codec that is missing
  instead of leaving it to be discovered later.

  Getting those codecs is the awkward part of a host install, and it is worth
  saying plainly: **Debian and Ubuntu do not ship an ImageMagick that can
  write JPEG XL.** Debian bookworm's is 6.9.11 from 2021 with no JPEG XL
  coder at all, and Debian trixie's 7.1.1 has AVIF and HEIC as read-only. A
  host install therefore means either building ImageMagick from source with
  `--with-jxl` and `--with-heic`, or taking the ImageMagick from Nixpkgs,
  which is the same build the development environment and the container image
  use. The container image is the easier of the two on a small host.
- **A private data directory**, mode `0700`, owned by the service account.
  Atlas refuses to start against a non-empty directory that group or other
  users can reach.

### The environment file

The token is a secret and lives in a file, not in a unit file and not on a
command line where it would show up in `ps`. `contrib/atlas.env.example` is a
starting point:

```sh
useradd --system --user-group --home-dir /var/lib/atlas \
    --no-create-home --shell /usr/sbin/nologin atlas
install -d -o atlas -g atlas -m 0750 /etc/atlas
install -o atlas -g atlas -m 0600 contrib/atlas.env.example /etc/atlas/atlas.env
$EDITOR /etc/atlas/atlas.env
```

```ini
ATLAS_API_TOKEN=the-long-random-string-openssl-rand-hex-32-gives
ATLAS_LISTEN_ADDR=127.0.0.1:3000
ATLAS_DATA_DIR=/var/lib/atlas
ATLAS_VARIANT_WIDTHS=320,640,960,1280
```

The token authorises every write and every metadata read, and it never
reaches a browser: the consuming application keeps it on its own server. The
widths belong in this file *before the first upload* — an asset keeps the
widths it was uploaded with, and
`POST /api/v1/images/{id}/renditions` adds the ones that are missing, so a
wrong setting is repairable but not free.

The other settings are described under [Configuration](#configuration). A
TOML file is the better place for the non-secret shape of a deployment; the
environment file is the better place for the token.

### The systemd unit

`contrib/atlas.service` runs the service as its own account, in the
foreground, with the environment file and the data directory handled:

```sh
install -m 0755 target/release/atlas /usr/local/bin/atlas
install -m 0644 contrib/atlas.service /etc/systemd/system/atlas.service
systemctl daemon-reload
systemctl enable --now atlas
```

The unit's `StateDirectory=atlas` creates `/var/lib/atlas` as the service
account owns it, with mode `0700`, before the first start. Nothing else has to
create it and nothing else has to remember to make it private — which is the
detail that is easy to get wrong and that Atlas refuses to start without.

### The container image

The `Dockerfile` builds the service and an ImageMagick that has the codecs.
Build it where there are cores and memory — a CI runner, a laptop, anything
bigger than the server it is going to run on.

The repository's Actions workflow builds and smoke-tests the image on every
change. After the PR that updates `Cargo.toml` is merged, tag that merged
commit to publish it to GitHub Container Registry. Use the SemVer version
from `Cargo.toml` verbatim, including its pre-release label; the workflow
checks that the tag matches. For the current `0.1.0-alpha.1` crate version:

```sh
jj git fetch --remote origin
jj tag set 0.1.0-alpha.1 -r master@origin
jj git push --tag 0.1.0-alpha.1
```

The server pulls the versioned image rather than compiling the Rust tree:

```sh
docker pull ghcr.io/yehoslav/atlas:0.1.0-alpha.1
```

The floating `latest` image tag is updated only by stable SemVer tags; an
alpha release never becomes `latest`.

The first published GHCR package may be private by default. To let the server
pull without credentials, open the package's **Package settings** on GitHub and
change its visibility to public. Otherwise, configure a read-only GitHub
token with `read:packages` and log in to `ghcr.io` on the server.

Pull a *version*, never a floating tag: a floating tag changes the bytes under
a running service, which is the same mistake as a mutable URL. A server with
no registry to talk to can carry the image as a file instead:

```sh
docker save ghcr.io/yehoslav/atlas:0.1.0-alpha.1 | gzip > atlas-0.1.0-alpha.1.tar.gz
# on the server
gunzip -c atlas-0.1.0-alpha.1.tar.gz | docker load
```

Copy `contrib/atlas.container.env.example` to `/etc/atlas/atlas.container.env`,
set the token, and make the file readable only by root. Run it with that file,
the data directory on a volume, and the port published to the host's loopback:

```sh
sudo install -o root -g root -m 0600 contrib/atlas.container.env.example /etc/atlas/atlas.container.env
sudoedit /etc/atlas/atlas.container.env

sudo docker run --detach --name atlas \
    --publish 127.0.0.1:3000:3000 \
    --env-file /etc/atlas/atlas.container.env \
    --volume atlas-data:/var/lib/atlas \
    ghcr.io/yehoslav/atlas:0.1.0-alpha.1
```

Two details worth knowing. A **named volume** inherits its ownership from the
image, so the service account can write to it; a **bind mount** takes the host
directory's ownership instead, and the service account is uid 100 and gid 101,
so the host directory has to be given to it (`chown 100:101`) or every upload
fails with a permission error. And the port is published to the host's
loopback on purpose: the media is meant to be public, but through the site's
own proxy, not through a port the whole internet can reach.

### The reverse proxy

Atlas expects the site's HTTPS proxy in front of it. The proxy routes
`/media/` publicly; the consuming application's backend calls the API on the
host's loopback directly, so the API and token never reach the browser:

```nginx
location /media/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
}

```

Proxy path matching must be case-preserving: asset IDs are lowercase, and a
proxy that folds case turns a valid ID into a `404`.

### The health check

`GET /healthz` answers `{"status":"ok"}` once the service is up. It is worth
more than a liveness ping, because the service only starts serving after it
has applied the schema migrations and run `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` — a database that fails either of those stops the
service from starting at all, with the reason in the log. A `200` from
`/healthz` therefore means the database is sound, not merely that a process
answers.

### Upgrading

The schema migrations run at startup, so stop the old process before replacing
the binary or image.

For a container deployment, bump the version in `Cargo.toml`, push that exact
SemVer tag (for example, `0.1.0-alpha.2`), then pull and
replace the container. Removing the container does not remove its named data
volume:

```sh
sudo docker pull ghcr.io/yehoslav/atlas:X.Y.Z-alpha.N
sudo docker stop atlas
sudo docker rm atlas
sudo docker run --detach --name atlas \
    --publish 127.0.0.1:3000:3000 \
    --env-file /etc/atlas/atlas.container.env \
    --volume atlas-data:/var/lib/atlas \
    ghcr.io/yehoslav/atlas:X.Y.Z-alpha.N
sudo docker logs atlas
```

For a systemd deployment:

```sh
systemctl stop atlas
install -m 0755 target/release/atlas /usr/local/bin/atlas
systemctl start atlas
journalctl -u atlas
```

For either deployment, finish with `curl -s localhost:3000/healthz`, which
should answer `{"status":"ok"}`.

The migrations are additive and guarded, so the failure mode is that the
service does not start and the log says why. A failed migration rolls back
that migration; earlier successfully applied migrations remain recorded. The
service refuses to serve rather than serving from a half-migrated database.
Fix the cause in the log and start the new version again. Nothing else is
needed around a successful upgrade: the files are unchanged, the URLs are
unchanged, and the assets are the same assets.

## Backup and restore

Metadata and files are useless separately: a restore that pairs a database with
a different set of `assets/` directories makes every asset undeliverable. Back
both up, and drill the restore.

**Snapshot the database with `VACUUM INTO`, never `cp`.** Atlas runs SQLite in
WAL mode, where committed transactions may still live in `atlas.sqlite3-wal`.
Copying `atlas.sqlite3` on its own captures neither the WAL nor a consistent
cut, and the result may be a database that opens fine and is missing rows.
`VACUUM INTO` takes a read lock, writes a single consistent file, and compacts
it as a side effect:

```sh
sqlite3 "$ATLAS_DATA_DIR/atlas.sqlite3" \
  "VACUUM INTO '/backup/atlas-$(date -u +%Y%m%dT%H%M%SZ).sqlite3'"
```

**SQLite does not `fsync` the `VACUUM INTO` output.** A crash or a full disk
during the write can leave a short file that still opens. A backup job that
assumes success is a backup that does not exist, so verify each snapshot:

```sh
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 'PRAGMA integrity_check;'
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 \
  'SELECT COUNT(*) FROM assets; SELECT COUNT(*) FROM renditions;
   SELECT COUNT(*) FROM superseded_renditions;'
```

`PRAGMA quick_check` is enough to catch a truncated file; `integrity_check` is
the stricter version. Keep the output, and compare the counts with the live
database (`SELECT COUNT(*) FROM assets`) at a quiet moment.

**Then copy the files.** A file copy only has to be internally consistent, so
`snapshot`, `rsync -a`, or `tar` are all fine:

```sh
rsync -a --delete "$ATLAS_DATA_DIR/assets/" /backup/assets/
rsync -a --delete "$ATLAS_DATA_DIR/trash/"  /backup/trash/
```

Snapshot the database *first*: the file copy taken afterwards may contain an
asset the database does not know about (harmless — startup recovery removes
unindexed directories) rather than missing one (broken delivery until the next
upload). Note that files copied while Atlas is running can be mid-upload; the
staging directory is never published, so ignore it and let startup recovery
clear it.

**Restore drill.** Stop Atlas, restore both halves, start it, and check:

1. `GET /healthz` returns `ok` — this already proves the migrations applied and
   both `PRAGMA` checks passed.
2. `GET /api/v1/images/{id}` for a known asset returns its metadata.
3. `GET /media/{id}/original` returns `200` with the expected length, not a
   placeholder.

A restore drill that has not fetched an actual file has not tested the thing
that matters.

## Status

Atlas is a prototype, and it says so in its API. The HTTP contract is pre-1.0
and may change in ways that break a consumer; the schema only ever moves
forward, additively, but the routes and the responses are not frozen.

The design record is [`PLAN.md`](PLAN.md), which holds the decisions and the
reasoning behind them, and [`TODO.md`](TODO.md), which holds what is still open
and what has already been settled. Both are worth reading before changing
anything structural.

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

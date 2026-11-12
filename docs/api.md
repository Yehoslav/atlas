# The HTTP API

This is the reference for Atlas's HTTP surface: every route, how a rendition
URL resolves, how uploads and lifecycle work, and what a consumer asks to
find out whether Atlas can still serve an asset. Atlas is a prototype, so
this contract is pre-1.0 and may change in ways that break a consumer; the
schema only ever moves forward, additively, but the routes and the responses
are not frozen. For what Atlas is and how to run it, see the
[`README`](../README.md).

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
- `POST /api/v1/images/re-render` — authenticated re-encode of selected images
  or a page of every live image. For a retry-safe targeted batch, send
  `{"ids":[{"id":"…","expect_generation":2}]}` (max 100 unique IDs).
  `expect_generation` is optional; when the current generation is already at
  least that value, the item is returned in `skipped` with its current
  generation. Otherwise Atlas re-encodes it once. To make a retry safe, set it
  to the listed generation plus one. Omit the expectation to always re-encode.
  Each item is independently reported in `re_encoded`,
  `skipped`, or `failures`; failure entries include an error code and message.
  The batch response has `next: null` and `remaining: 0`.

  To walk the whole live collection, send `{"limit":10,"after":"…"}`;
  both fields are optional and `limit` is capped at 100. `next` is an opaque,
  versioned cursor token to pass back as `after`, and is `null` when the walk is
  done. The cursor advances past failures, so collect them as you go. Trashed
  images fail as individual items; restore one before retrying it. Use explicit
  IDs with expected generations when a lost response must be safely retried
  without re-encoding completed assets.

  With `"async": true` the same selection runs as a job instead of holding the
  request open: the answer is `202` with a `job_id`, returned as soon as the
  selection is validated and stored, and processing continues with no connection
  attached — a disconnect, timeout or reload cannot cancel it. Async accepts
  the same `ids` batch (up to 1000 items) or a whole-collection walk (with an
  optional `after`; `limit` does not apply, because a job processes its whole
  selection). Poll `GET /api/v1/images/re-render/{job_id}` for `state`
  (`queued`, `running`, `completed`, `failed`), `progress` (`total`,
  `processed`, `re_encoded`, `skipped`, `failed`) and `results`: one entry per
  selected asset with its `outcome`, its `generation` when known, and a
  structured `error` for failures. Job rows are SQLite state, so results
  survive an Atlas restart; a job interrupted mid-run is picked up at startup
  from its pending items; finished jobs are pruned after the trash retention
  window. A job-level `error` is set only in the `failed` state — per-image
  failures are results, not job failures.
- `GET /api/v1/images/re-render/{job_id}` — authenticated status of one
  re-render job, as described above. An unknown or malformed id is `404
  job_not_found`.

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
needs it. `POST /api/v1/images/re-render` walks live images a page per call;
pass the opaque `next` token back as `after` until it is `null`. The cursor
moves past an image that failed, so `null` means every live image was tried,
not that every one was re-encoded. Keep the `failures` of each page and retry
failed IDs after fixing the cause.

An unconditional re-encode is not free or idempotent. Every one writes a new
copy of the asset's renditions and keeps every older copy until the asset is
purged, so the disk an image's renditions take grows by one copy per re-encode,
and the consumer's stored srcsets change each time. For retry safety, send
explicit IDs with `expect_generation` set to the desired next generation. If the
response is lost, retry that batch: assets already at or above that generation
are returned in `skipped` with their current generation instead of being
re-encoded again.

A page may re-encode several images concurrently, bounded by
`ATLAS_CONVERSION_CONCURRENCY` (default 1). Behind a proxy, keep `limit` small
enough that the page finishes inside the proxy's read timeout (nginx waits 60
seconds by default): a page may have re-encoded some images when the proxy
gives up. Use explicit IDs and expected generations to retry safely.

For a run too large for any proxy to wait through — a browser-driven
regeneration of a few hundred assets — start it as a job: send the same body
with `"async": true`, get `202` and a `job_id` in milliseconds, and poll
progress instead of holding a request open. The connection that asked for the
job has no further say over it; only `GET /api/v1/images/re-render/{job_id}`
does.

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

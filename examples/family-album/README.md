# Family Album

A small Flask application that consumes Atlas the way a real site would. It
exists to prove the integration contract from the consumer's side, so it is a
reference implementation: readable source, one dependency file, and no
production ambitions.

The split is the point. **Atlas owns the media bytes. This application owns the
meaning** — descriptions, alt text, tags, folders, and the posts that reference
photos. Neither service stores the other's data. If a photo row is lost, the
asset is still in Atlas; if an asset is purged, the album's captions are still
here.

## What it demonstrates

- **Its own database.** `schema.sql` holds folders, photos, renditions, posts,
  and the post/photo join. Not one of those tables exists in Atlas. The one
  exception is `photos.kind`, which records *which Atlas collection* the asset
  lives in: not meaning, just the address of the API route the album needs for
  the asset's whole life.
- **A server-side Atlas client.** `atlas_client.py` is the only thing that knows
  the API token. It streams uploads rather than buffering them, and it is never
  imported by anything the browser sees.
- **Sequential uploads with per-file results.** Atlas takes two concurrent
  uploads but converts one image at a time, so a burst of ten earns a `503` from
  half of them. This uploads one at a time and reports every file's outcome,
  and says *why* a file was refused: a camera raw file is told to be converted,
  anything else is told which types the album handles. Telling every rejected
  file it was raw was telling a person with a `.txt` nothing at all.
- **Uploading a name.** The album percent-encodes the file name and sends it as
  `X-Original-Filename`, so a download of a PDF is called what the person named
  it instead of `asset.pdf`.
- **Deduplication that reports itself.** Uploading identical bytes returns the
  asset Atlas already has, and the album points at the photo row it already
  wrote instead of failing on its own `UNIQUE` constraint.
- **Metadata editing that never calls Atlas.** A caption is the album's business.
- **A referential guard.** Before deleting, the album checks its own posts for
  published references and returns `409` with a confirm button. Atlas cannot do
  this: it has no idea which of its assets a published page shows. The photo's
  own "Used by" panel lists *every* post that uses it, drafts included and
  badged as such — the guard filters on published because that is what
  protects a reader, while "Used by" is a statement about the photo and has
  to be true.
- **Usage registration.** Attaching a photo to a post pins the asset in Atlas
  (`PUT /api/v1/images/{id}/pin`), so the retention sweep will never discard
  bytes a reader can currently see.
- **Renditions used properly.** `w320` for thumbnails, `w640`/`w960` for the
  grid's `srcset`, and the widest stored rendition for the full-size view. The
  widths come from the upload response and are stored, so the album never asks
  Atlas to transform anything on a read. Atlas treats `/wN` as a maximum width
  and never upscales, so a `srcset` descriptor is the rendition's **actual**
  width while its URL stays pointed at the profile Atlas stored it under.
- **Graceful degradation.** The album asks Atlas about each photo it is about to
  render, and renders either the image or an empty state that says *why*: live,
  deleted in Atlas (still restorable), purged (gone for good), unknown to Atlas,
  or Atlas unreachable. The reason matters — "deleted" has a way back and
  "purged" does not. A small `error`-listener catches the one race left: an
  asset retired between the check and the browser's request.
- **Saying which collection, not guessing.** Atlas's routes are typed
  (`/api/v1/images` vs `/api/v1/documents`), so the album records the collection
  with each photo and never infers it later. The upload form's "store as
  documents" box is the visible consequence: a screenshot or a diagram is kept
  byte-for-byte with no renditions and served as a download, because
  re-encoding it would blur the very text the reader needs.

## Two processes, not one

Atlas and the album are separate services. The browser only ever talks to the
album, except for `/media/` image bytes, which the site's reverse proxy serves
straight from Atlas.

```
browser ──► reverse proxy ──┬──► /            → album (Flask, :5000)
                            └──► /media/...   → Atlas   (:3000)
                                      ▲
                        album ─────────┘  HTTP + Bearer token, server-side only
```

## Running it

Two terminals. Atlas first:

```sh
export ATLAS_API_TOKEN='a-long-random-secret'
export ATLAS_DATA_DIR="$PWD/atlas-data"
cargo run --manifest-path ../..    # or run from the Atlas checkout
```

Then the album, from this directory:

```sh
python3 -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt

export ATLAS_API_TOKEN='the-same-long-random-secret'
export ATLAS_API_URL='http://127.0.0.1:3000'   # where the album reaches Atlas
export ATLAS_MEDIA_URL='http://127.0.0.1:3000' # where the browser reaches /media/
python app.py
```

Or put those in a file, so they do not have to be exported every time:

```sh
cp .env.example .env
$EDITOR .env
python app.py
```

The album reads `.env` from beside this module, or from the path in
`ALBUM_ENV_FILE`. Its values only fill in what the environment does not
already provide, so an exported variable still wins.

Open <http://127.0.0.1:5000>.

### Where the token lives

In the album process's environment, and nowhere else. `atlas_client.py` reads
`ATLAS_API_TOKEN`; the templates never see it, no route ever accepts it from the
browser, and the album's SQLite database does not contain it. `ATLAS_API_TOKEN`
belongs to Atlas's environment, not to a file — Atlas deliberately refuses to
read it from a config file so a committed config can never carry a secret.

### Which URLs are public

| URL | Served by | Authenticated |
| --- | --- | --- |
| `/`, `/photos/…`, `/upload`, `/posts`, `/trash` | album | no (there is no login here) |
| `/media/{id}/original`, `/media/{id}/w{width}` | Atlas | no, deliberately public |
| `/api/v1/images…`, `/api/v1/documents…` | Atlas | yes, Bearer token |

This example has no accounts: it is a single-family reference app, and adding
login would only hide the integration. A real site puts its own authentication
in front of these album routes and keeps the Atlas token behind them.

### Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `ATLAS_API_TOKEN` | required | The Atlas token. Server-side only. |
| `ATLAS_API_URL` | `http://127.0.0.1:3000` | Where the album sends API calls |
| `ATLAS_MEDIA_URL` | same origin as `ATLAS_API_URL` | Prefix for `/media/` URLs in HTML. Leave empty when the reverse proxy serves Atlas on the same origin, which is the normal deployment. |
| `ALBUM_DATABASE` | `./album.sqlite3` | The album's own database |
| `ALBUM_MAX_UPLOAD_BYTES` | 50 MiB | Per-file ceiling checked before uploading |
| `ALBUM_MAX_REQUEST_BYTES` | 200 MiB | Whole-request ceiling for a bulk upload |
| `ALBUM_HOST` / `ALBUM_PORT` | `127.0.0.1` / `5000` | Where the album listens |

## Availability, and what it costs

The album cannot know on its own whether Atlas will still serve an asset: Atlas
gives no way to ask "is this id still there?" without naming it, and its whole
design is that it forgets files without telling anyone. So it asks, once per
page, through
`POST /api/v1/assets/state` with every asset ID on that page — one call, up to
500 ids, answered from the request cache so a photo rendered twice costs
nothing. Before that endpoint existed this was one call per photo, which is the
kind of arithmetic that looks fine in a test with three photos and does not
survive a real album.

That call buys the only thing worth having here: the album never renders a
broken image, and when it cannot show one it says which of five things happened.

| State | What the page shows |
| --- | --- |
| `active` | The image |
| `deleted` | "Deleted in Atlas. The bytes are still in the trash and can be restored", with a Restore button on the photo page — or, if the album is the one that deleted it, "You deleted this photo from the album on *date*", because that half of the story only the album knows |
| `purged` | "Purged by Atlas after its retention window. The bytes are gone for good" |
| `404` | "Atlas has no record of this asset" |
| Atlas down | "Atlas could not be reached, so whether it can serve this photo is unknown" |

## Routes

| Route | What it does |
| --- | --- |
| `GET /` | The grid, filterable by folder and by one of the album's own tags |
| `GET /upload`, `POST /upload` | Single or bulk upload with a per-file report |
| `GET /photos/<id>` | Full-size view, metadata form, references, delete guard |
| `POST /photos/<id>` | Save description, alt text, tags, folder — album database only |
| `POST /photos/<id>/delete` | Move the asset to Atlas's trash, after the referential guard |
| `POST /photos/<id>/pin` | Toggle Atlas's purge pin by hand |
| `POST /photos/<id>/attach` | Add this photo to a post and pin the asset |
| `GET /posts` | Posts, publishing, and the photos in a post |
| `GET /trash` | The photos this album deleted, each with what Atlas still has |
| `POST /trash/<collection>/<asset_id>/restore` | Restore one |

## Deleting, end to end

1. The album checks its own `posts` table. A published post referencing the
   photo returns `409` and names the post; nothing is deleted without
   `confirm=yes`.
2. `DELETE /api/v1/images/{id}` moves the bytes into Atlas's trash. Atlas stamps
   the delete *before* the files move, so the public URLs start serving a
   placeholder immediately.
3. The album stamps `photos.deleted_at` and **keeps the row**. It has to: the
   description, alt text, tags and folder exist only here, and no amount of
   asking Atlas could rebuild them. A photo leaves the grid and the folder and
   tag views, and its own page keeps working, saying the album deleted it and
   when.
4. `POST /trash/<collection>/{id}/restore` clears that stamp and tells Atlas to
   put the bytes back at the same URLs. The collection comes from the album's
   own row, recorded at upload — guessing it from the content type would send a
   picture stored as a document to the images route, where it is not found.
5. If nobody restores it, Atlas purges it after `ATLAS_TRASH_RETENTION_DAYS` and
   keeps a tombstone. The album's row survives and can now say *the bytes are
   gone for good*, which is a fact only Atlas has.

The two services hold different halves of this and the pages show both rather
than pretending either is the whole truth: the album knows *when you deleted
it*, Atlas knows *whether the bytes still exist*. That is the point of an
example — the album's trash page lists its own deletions, not everything in
Atlas's trash, because an asset uploaded by a command line was never in the
album and could not be put back into it.

## Tests

```sh
pip install -r requirements-dev.txt
python -m pytest tests -q
ruff check .
ruff format --check .
ty check
```

The tests replace Atlas with an in-memory fake, so they need no running service.

Two layers of fake back this up, and they are not the same thing. `test_app.py`
runs the album's own views against `FakeAtlas`, so it tests the album. The
`ScriptedSession` tests in `test_atlas_client.py` run the *real* client against
a recorded transport, so they test the URLs the client actually builds. That
layer is what catches a client disagreeing with Atlas about the API, which a
fake-backed test cannot see.

`ty check` needs the virtualenv active. It is worth running: it found a method
called `list` shadowing the builtin and quietly corrupting every later type
annotation in that file, and a fake calling a method it did not have from a
branch that no test reached.
They cover the upload report, the rendition widths the album renders, metadata
edits that never reach Atlas, the referential guard, pin registration, and what
the pages do when an asset has gone missing. `tests/test_atlas_client.py` covers
the client itself, including that a stopped Atlas arrives as an explanation
rather than a traceback.

The source is formatted with [`ruff`](https://docs.astral.sh/ruff/) if you have
it: `ruff format . && ruff check .`

## Deliberately out of scope

- Atlas-side listing, search, or tags. The album's own database covers that,
  and the separation is the point of the exercise.
- Authentication or multiple users.
- Anything that would push Atlas toward knowing about albums, posts, or people.

"""Family Album: a small Flask site that consumes Atlas.

The split is the whole point. Atlas stores the bytes and knows nothing about
family history; this application owns its own SQLite database of folders,
descriptions, alt text, tags, and posts, and asks Atlas for media through its
HTTP API. Everything Atlas needs to be told happens server-side, with the token
held in this process.
"""

from __future__ import annotations

import os
import secrets
import sqlite3
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from flask import (
    Flask,
    abort,
    flash,
    g,
    redirect,
    render_template,
    request,
    url_for,
)
from werkzeug import Response
from werkzeug.exceptions import HTTPException

import album_db
from atlas_client import AtlasClient, AtlasError, content_type_for

HERE = Path(__file__).parent

#: Atlas rejects a document over 50 MiB and an image over 20 MiB. Checking here
#: turns a wasted upload into a sentence in the upload report.
DEFAULT_MAX_UPLOAD_BYTES = 50 * 1024 * 1024

#: A bulk request carries several files, so the request cap is a multiple of the
#: per-file cap. Both are about not filling the disk, not about Atlas's limits.
DEFAULT_MAX_REQUEST_BYTES = 4 * DEFAULT_MAX_UPLOAD_BYTES

#: Widths the album asks for. 320 for thumbnails, 640 and 960 for the grid's
#: srcset, and the widest stored rendition for a full-size view. Atlas only ever
#: has the widths it pre-generated, so the album uses what came back.
THUMBNAIL_WIDTH = 320
GRID_WIDTHS = (640, 960)


@dataclass
class UploadResult:
    filename: str
    ok: bool
    message: str
    asset_id: str | None = None
    photo_id: int | None = None


#: What the album shows instead of an image, and why. "Why" matters: a photo
#: that was deleted is recoverable and a photo that was purged is not, and an
#: unreachable Atlas says nothing about either.
ASSET_REASONS = {
    "deleted": "Deleted in Atlas. The bytes are still in the trash and can be restored.",
    "purged": "Purged by Atlas after its retention window. The bytes are gone for good.",
    "missing": (
        "Atlas has no record of this asset: it was never uploaded there, or it "
        "was removed with the media directory."
    ),
    "unreachable": (
        "Atlas could not be reached, so whether it can serve this photo is unknown."
    ),
}

#: Shown when Atlas stops serving an image between the check below and the
#: browser's request for it.
FALLBACK_REASON = "Atlas stopped serving this photo while the page was loading."


@dataclass(frozen=True)
class AssetCheck:
    """What Atlas can tell this album about one asset right now."""

    status: str
    asset: Any = None

    @property
    def live(self) -> bool:
        # Atlas calls it `active`; the album says `live` to avoid two words for
        # one thing. `deleted` and `purged` are Atlas's own words, kept as-is.
        return self.status == "active"

    @property
    def reason(self) -> str:
        return ASSET_REASONS.get(self.status, "")


def human_bytes(count: int) -> str:
    size = float(count)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if size < 1024 or unit == "GiB":
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} GiB"


def format_timestamp(value: int | None) -> str:
    """Atlas reports Unix seconds for every lifecycle stamp."""
    if not value:
        return "unknown"
    moment = datetime.fromtimestamp(value, tz=UTC)
    return moment.strftime("%Y-%m-%d %H:%M UTC")


class AlbumImages:
    """Turns a stored photo into `<img>` attributes.

    Every URL here is Atlas's, and every width is one Atlas reported at upload
    time. The album never asks Atlas to transform anything on the fly.
    """

    def __init__(self, atlas: AtlasClient) -> None:
        self._atlas = atlas

    def _widths(self, photo: album_db.Photo) -> list[int]:
        return [rendition.max_width for rendition in photo.renditions]

    def rendition_url(self, photo: album_db.Photo, max_width: int) -> str:
        return self._atlas.rendition_url(photo.asset_id, max_width)

    def original_url(self, photo: album_db.Photo) -> str:
        return self._atlas.original_url(photo.asset_id)

    def srcset(self, photo: album_db.Photo, widths: Iterable[int]) -> str:
        """A srcset of stored widths, described by their real output width.

        A profile is a maximum, not a promise: Atlas never upscales, so a
        500-pixel photo stores one rendition under several profiles, and the
        descriptor has to be the width that was actually produced or the
        browser will reserve space for an image that does not exist.
        """
        by_profile = {rendition.max_width: rendition for rendition in photo.renditions}
        chosen = [by_profile[width] for width in widths if width in by_profile]
        if not chosen and by_profile:
            chosen = [min(by_profile.values(), key=lambda rendition: rendition.width)]
        return ", ".join(
            f"{self.rendition_url(photo, rendition.max_width)} {rendition.width}w"
            for rendition in chosen
        )

    def srcset_all(self, photo: album_db.Photo) -> str:
        """Every width Atlas stored, for a view that can show any of them."""
        return self.srcset(photo, self._widths(photo))

    def grid_src(self, photo: album_db.Photo) -> str:
        available = self._widths(photo)
        for width in GRID_WIDTHS:
            if width in available:
                return self.rendition_url(photo, width)
        if available:
            return self.rendition_url(photo, available[0])
        # A PDF has no renditions; show the original rather than nothing.
        return self.original_url(photo)

    def thumbnail_url(self, photo: album_db.Photo) -> str:
        if not photo.is_image:
            return self.original_url(photo)
        available = self._widths(photo)
        if THUMBNAIL_WIDTH in available:
            return self.rendition_url(photo, THUMBNAIL_WIDTH)
        return (
            self.rendition_url(photo, available[0])
            if available
            else self.original_url(photo)
        )

    def full_size(self, photo: album_db.Photo) -> tuple[str, int | None, int | None]:
        """The widest stored rendition, or the original when there are none.

        Renditions are never upscaled, so a photo narrower than the widest
        profile comes back with a smaller `width`, and this returns that.
        """
        available = self._widths(photo)
        if not available:
            return self.original_url(photo), photo.width, photo.height
        widest = max(available)
        rendition = next(r for r in photo.renditions if r.max_width == widest)
        return self.rendition_url(photo, widest), rendition.width, rendition.height


def create_app(
    atlas: AtlasClient | None = None,
    database: str | Path | None = None,
) -> Flask:
    app = Flask(__name__)
    atlas = atlas or AtlasClient.from_env()
    database_path = Path(
        database or os.environ.get("ALBUM_DATABASE", HERE / "album.sqlite3")
    )
    max_upload_bytes = int(
        os.environ.get("ALBUM_MAX_UPLOAD_BYTES", DEFAULT_MAX_UPLOAD_BYTES)
    )

    app.config.update(
        DATABASE=str(database_path),
        SECRET_KEY=os.environ.get("ALBUM_SECRET_KEY", secrets.token_hex(16)),
        MAX_CONTENT_LENGTH=int(
            os.environ.get("ALBUM_MAX_REQUEST_BYTES", DEFAULT_MAX_REQUEST_BYTES)
        ),
    )

    # The album owns this schema and creates it if it is missing. Atlas has no
    # part in it.
    with album_db.connect(database_path) as bootstrap:
        album_db.init(bootstrap)

    def get_db() -> sqlite3.Connection:
        if "db" not in g:
            g.db = album_db.connect(database_path)
        return g.db

    @app.teardown_appcontext
    def close_db(exception: BaseException | None) -> None:
        connection: sqlite3.Connection | None = g.pop("db", None)
        if connection is not None:
            connection.close()

    def check_asset(asset_id: str) -> AssetCheck:
        """Ask Atlas once per request what it can serve for this asset.

        The album cannot know on its own: Atlas has no listing endpoint, so the
        only way to learn that an asset was deleted, purged, or lost is to ask.
        One request per photo on the page, cached so a photo rendered twice
        costs one call.
        """
        cache = g.setdefault("asset_checks", {})
        if asset_id not in cache:
            cache[asset_id] = fetch_asset(atlas, asset_id)
        return cache[asset_id]

    images = AlbumImages(atlas)
    app.jinja_env.globals.update(
        atlas=atlas,
        images=images,
        check_asset=check_asset,
        fallback_reason=FALLBACK_REASON,
        human_bytes=human_bytes,
        timestamp=format_timestamp,
        max_upload_bytes=max_upload_bytes,
        grid_widths=GRID_WIDTHS,
        thumbnail_width=THUMBNAIL_WIDTH,
    )

    @app.get("/")
    def index() -> str | Response:
        connection = get_db()
        folder_id = request.args.get("folder", type=int)
        photos = album_db.list_photos(
            connection, folder_id=folder_id, tag=request.args.get("tag") or None
        )
        return render_template(
            "index.html",
            photos=photos,
            folders=album_db.list_folders(connection),
            folder=album_db.get_folder(connection, folder_id) if folder_id else None,
            tag=request.args.get("tag") or None,
        )

    @app.route("/upload", methods=["GET", "POST"])
    def upload() -> str:
        if request.method == "GET":
            return render_template("upload.html", results=[])

        uploads = [item for item in request.files.getlist("photos") if item.filename]
        if not uploads:
            flash("Choose at least one photo to upload.", "error")
            return redirect(url_for("upload"))

        # Strictly one at a time. Atlas takes two concurrent uploads but runs a
        # single conversion, so a burst of ten would earn a 503 from half of
        # them; sequential uploads are slower and boring, which is the point.
        results = [
            _upload_one(get_db(), atlas, item, max_upload_bytes) for item in uploads
        ]
        return render_template("upload.html", results=results)

    @app.get("/photos/<int:photo_id>")
    def photo_detail(photo_id: int) -> str:
        connection = get_db()
        photo = album_db.get_photo(connection, photo_id) or abort(404)
        return render_template(
            "photo.html",
            photo=photo,
            references=album_db.references_for_photo(connection, photo_id),
            posts=album_db.list_posts(connection),
            folders=album_db.list_folders(connection),
        )

    @app.post("/photos/<int:photo_id>")
    def edit_photo(photo_id: int) -> Response:
        album_db.get_photo(get_db(), photo_id) or abort(404)
        folder_id = request.form.get("folder_id", type=int)
        # Only the album's database is touched. Atlas is not told that a caption
        # changed, because Atlas has no idea what a caption is.
        album_db.update_photo(
            get_db(),
            photo_id,
            description=request.form.get("description", ""),
            alt_text=request.form.get("alt_text", ""),
            tags=request.form.get("tags", ""),
            folder_id=folder_id,
        )
        flash("Saved.", "ok")
        return redirect(url_for("photo_detail", photo_id=photo_id))

    @app.post("/photos/<int:photo_id>/delete")
    def delete_photo(photo_id: int) -> tuple[str, int]:
        connection = get_db()
        photo = album_db.get_photo(connection, photo_id) or abort(404)
        references = album_db.references_for_photo(connection, photo_id)

        if references and request.form.get("confirm") != "yes":
            # The referential guard. Atlas cannot know that "Grandma's 70th" is
            # published and shows this photo, so the album checks its own posts
            # and asks a person before it deletes anything.
            return (
                render_template(
                    "photo.html",
                    photo=photo,
                    blocking=references,
                    references=references,
                    posts=album_db.list_posts(connection),
                    folders=album_db.list_folders(connection),
                ),
                409,
            )

        try:
            atlas.delete(photo.asset_id)
        except AtlasError as error:
            flash(f"Atlas refused to delete the asset: {error.message}", "error")
            return redirect(url_for("photo_detail", photo_id=photo_id))

        album_db.delete_photo(connection, photo_id)
        if references:
            flash(
                "Moved to the Atlas trash. Published posts still point at it and will show"
                " a placeholder until the asset is restored or Atlas purges it.",
                "warn",
            )
        else:
            flash("Moved to the Atlas trash. Restore it from the trash page.", "ok")
        return redirect(url_for("index"))

    @app.post("/photos/<int:photo_id>/pin")
    def toggle_pin(photo_id: int) -> str:
        photo = album_db.get_photo(get_db(), photo_id) or abort(404)
        asset = check_asset(photo.asset_id).asset
        wanted = not (asset.pinned if asset else False)
        try:
            atlas.set_pinned(photo.asset_id, wanted)
        except AtlasError as error:
            flash(f"Atlas refused to change the pin: {error.message}", "error")
        else:
            flash(
                "Pinned: Atlas will not purge this asset." if wanted else "Unpinned.",
                "ok",
            )
        return redirect(url_for("photo_detail", photo_id=photo_id))

    @app.get("/posts")
    def posts() -> str:
        connection = get_db()
        post_id = request.args.get("post", type=int)
        return render_template(
            "posts.html",
            posts=album_db.list_posts(connection),
            selected=album_db.get_post(connection, post_id) if post_id else None,
            photos=album_db.post_photos(connection, post_id) if post_id else [],
        )

    @app.post("/posts")
    def create_post() -> str:
        title = request.form.get("title", "").strip()
        if not title:
            flash("A post needs a title.", "error")
            return redirect(url_for("posts"))
        post_id = album_db.create_post(get_db(), title, request.form.get("body", ""))
        flash("Post created. It stays a draft until you publish it.", "ok")
        return redirect(url_for("posts", post=post_id))

    @app.post("/posts/<int:post_id>/publish")
    def publish_post(post_id: int) -> str:
        connection = get_db()
        album_db.get_post(connection, post_id) or abort(404)
        album_db.set_post_published(
            connection, post_id, request.form.get("publish") == "yes"
        )
        return redirect(url_for("posts", post=post_id))

    @app.post("/posts/<int:post_id>/photos/<int:photo_id>")
    def toggle_post_photo(post_id: int, photo_id: int) -> str:
        connection = get_db()
        album_db.get_post(connection, post_id) or abort(404)
        photo = album_db.get_photo(connection, photo_id) or abort(404)

        attached = bool(
            connection.execute(
                "SELECT 1 FROM post_photos WHERE post_id = ? AND photo_id = ?",
                (post_id, photo_id),
            ).fetchone()
        )
        if attached:
            album_db.detach_photo(connection, post_id, photo_id)
        else:
            album_db.attach_photo(connection, post_id, photo_id)

        _sync_pin(connection, atlas, photo, flash_message=True)
        return redirect(request.referrer or url_for("photo_detail", photo_id=photo_id))

    @app.post("/photos/<int:photo_id>/attach")
    def attach_to_post(photo_id: int) -> str:
        connection = get_db()
        photo = album_db.get_photo(connection, photo_id) or abort(404)
        post_id = request.form.get("post_id", type=int)
        if post_id is None:
            flash("Choose a post first.", "error")
            return redirect(url_for("photo_detail", photo_id=photo_id))
        album_db.get_post(connection, post_id) or abort(404)
        album_db.attach_photo(connection, post_id, photo_id)
        _sync_pin(connection, atlas, photo, flash_message=True)
        return redirect(url_for("photo_detail", photo_id=photo_id))

    @app.get("/trash")
    def trash() -> str:
        try:
            entries, error = atlas.trash(), None
        except AtlasError as failure:
            entries, error = [], failure.message
        return render_template("trash.html", entries=entries, atlas_error=error)

    @app.post("/trash/<asset_id>/restore")
    def restore(asset_id: str) -> str:
        try:
            atlas.restore(asset_id)
        except AtlasError as error:
            flash(f"Atlas refused to restore that asset: {error.message}", "error")
        else:
            flash("Restored. Its public URLs serve the original bytes again.", "ok")
        return redirect(url_for("trash"))

    @app.post("/folders")
    def create_folder() -> str:
        name = request.form.get("name", "").strip()
        if not name:
            flash("A folder needs a name.", "error")
        else:
            album_db.create_folder(get_db(), name)
            flash(f"Created folder {name}.", "ok")
        return redirect(request.referrer or url_for("index"))

    @app.post("/folders/<int:folder_id>/delete")
    def delete_folder(folder_id: int) -> str:
        album_db.delete_folder(get_db(), folder_id)
        flash("Folder removed. Its photos are still in the album.", "ok")
        return redirect(url_for("index"))

    @app.errorhandler(HTTPException)
    def handle_http_error(error: HTTPException):
        if error.code == 404:
            return render_template("error.html", message="That page is not here."), 404
        return render_template("error.html", message=error.description), error.code

    return app


def _upload_one(
    connection: sqlite3.Connection,
    atlas: AtlasClient,
    storage: Any,
    max_bytes: int,
) -> UploadResult:
    filename = storage.filename
    content_type = content_type_for(filename)
    if content_type is None:
        return UploadResult(
            filename,
            False,
            "Atlas does not accept this file type. Convert camera RAW to JPEG first.",
        )

    size = _stream_size(storage.stream)
    if size is None:
        return UploadResult(filename, False, "Could not read the file's size.")
    if size > max_bytes:
        return UploadResult(
            filename,
            False,
            f"{human_bytes(size)} is over the {human_bytes(max_bytes)} limit.",
        )

    try:
        asset = atlas.upload(storage.stream, content_type)
    except AtlasError as error:
        return UploadResult(filename, False, error.message)

    photo_id = album_db.add_photo(connection, asset)
    widths = (
        ", ".join(str(rendition.max_width) for rendition in asset.renditions) or "none"
    )
    return UploadResult(
        filename,
        True,
        f"Uploaded as {asset.id} (renditions: {widths}).",
        asset_id=asset.id,
        photo_id=photo_id,
    )


def fetch_asset(atlas: AtlasClient, asset_id: str) -> AssetCheck:
    """One Atlas metadata lookup, with every outcome named."""
    try:
        asset = atlas.get_asset(asset_id)
    except AtlasError:
        return AssetCheck("unreachable")
    if asset is None:
        return AssetCheck("missing")
    return AssetCheck(asset.state, asset)


def _sync_pin(
    connection: sqlite3.Connection,
    atlas: AtlasClient,
    photo: album_db.Photo,
    flash_message: bool,
) -> None:
    """Usage registration: keep an asset out of the trash while a post uses it.

    Atlas will never ask what a post is, so this one boolean is the entire
    contract between the two services.
    """
    wanted = album_db.photo_is_referenced(connection, photo.id)
    try:
        atlas.set_pinned(photo.asset_id, wanted)
    except AtlasError as error:
        if flash_message:
            flash(f"Could not update the Atlas pin: {error.message}", "error")


def _stream_size(stream: Any) -> int | None:
    """Size of an uploaded file without reading it into memory."""
    try:
        current = stream.tell()
        stream.seek(0, os.SEEK_END)
        size = stream.tell()
        stream.seek(current)
    except (AttributeError, OSError, ValueError):
        return None
    return int(size)


if __name__ == "__main__":
    # For development only. In production this sits behind the same reverse
    # proxy that serves Atlas's /media/ paths.
    create_app().run(
        host=os.environ.get("ALBUM_HOST", "127.0.0.1"),
        port=int(os.environ.get("ALBUM_PORT", "5000")),
    )

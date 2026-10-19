"""The album's own tests.

Atlas is replaced by an in-memory fake that speaks the same small interface, so
these tests exercise the integration contract - what the album stores, what it
sends, and what it does when an asset is missing - without a running Atlas.
"""

from __future__ import annotations

import dataclasses
import hashlib
import io
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import album_db
from app import collection_for, create_app
from atlas_client import (
    Asset,
    AssetState,
    AtlasError,
    Rendition,
    TrashEntry,
)

PNG_BYTES = b"\x89PNG\r\n\x1a\n not really a png"


class FakeAtlas:
    """Stands in for one Atlas instance."""

    def __init__(self) -> None:
        self.assets: dict[str, Asset] = {}
        self.deleted: list[str] = []
        self.restored: list[str] = []
        self.pins: dict[str, bool] = {}
        self.next_id = 0
        self.fail_next_upload: str | None = None
        self.state_calls: list[list[str]] = []
        self.file_names: list[str | None] = []
        self.by_digest: dict[str, Asset] = {}
        self.file_names: list[str | None] = []
        # Source width of the next upload. Atlas caps renditions at the source,
        # so a small photo stores narrower images than its profiles ask for.
        self.source_width = 1600

    def _make(
        self,
        state: str = "active",
        pinned: bool = False,
        file_name: str | None = None,
        kind: str = "images",
        content_type: str = "image/png",
    ) -> Asset:
        self.next_id += 1
        asset_id = f"{self.next_id:018d}"
        source = self.source_width
        is_image = content_type.startswith("image/")
        asset = Asset(
            id=asset_id,
            # Atlas spells the collection in JSON as a singular noun, while the
            # route uses the plural. The fake says it the way the server does, or
            # it stops being able to catch a mismatch.
            kind={"images": "image", "documents": "document"}[kind],
            created_at=1_700_000_000,
            deduplicated=False,
            state=state,
            pinned=pinned,
            deleted_at=None if state == "active" else 1_700_000_100,
            purged_at=None,
            content_type=content_type,
            bytes=2048,
            sha256="deadbeef",
            width=source if is_image else None,
            height=int(source * 3 / 4) if is_image else None,
            original_url=f"/media/{asset_id}/original",
            # Documents are stored whole: no renditions at all.
            renditions=tuple(
                Rendition(
                    max_width=profile,
                    width=min(profile, source),
                    height=int(min(profile, source) * 3 / 4),
                    formats=("webp", "png"),
                )
                for profile in (320, 640, 960, 1280)
                if kind == "images"
            ),
        )
        self.assets[asset_id] = asset
        return asset

    # --- the interface app.py uses -----------------------------------------

    def upload(
        self,
        stream,
        content_type: str,
        file_name: str | None = None,
        collection: str | None = None,
    ) -> Asset:
        if self.fail_next_upload:
            message, self.fail_next_upload = self.fail_next_upload, None
            raise AtlasError(message, status=415)
        collection = collection or collection_for(content_type)
        digest = f"{collection}:{hashlib.sha256(stream.read()).hexdigest()}"
        stream.seek(0)
        if digest in self.by_digest:
            existing = self.by_digest[digest]
            return dataclasses.replace(existing, deduplicated=True)
        self.file_names.append(file_name)
        asset = self._make(
            file_name=file_name, kind=collection, content_type=content_type
        )
        self.by_digest[digest] = asset
        return asset

    def get_asset(self, collection: str, asset_id: str) -> Asset | None:
        asset = self.assets.get(asset_id)
        # A typed route: an asset in the other collection is not found.
        return asset if asset and asset.collection == collection else None

    def states(self, asset_ids: list[str]) -> dict[str, AssetState]:
        self.state_calls.append(list(asset_ids))
        found = {}
        for asset_id in asset_ids:
            asset = self.assets.get(asset_id)
            found[asset_id] = AssetState(
                id=asset_id,
                state=asset.state if asset else "missing",
                pinned=asset.pinned if asset else False,
            )
        return found

    def delete(self, collection: str, asset_id: str) -> None:
        # Atlas does not destroy anything on request: the asset stays, marked
        # deleted, until its retention window passes. A fake that popped it out
        # here would hide every question about what a deleted photo looks like.
        self.deleted.append(asset_id)
        asset = self.assets.get(asset_id)
        if asset is not None and asset.collection == collection:
            self.assets[asset_id] = dataclasses.replace(
                asset, state="deleted", deleted_at=1_700_000_100
            )

    def restore(self, collection: str, asset_id: str) -> None:
        self.restored.append(asset_id)
        asset = self.assets.get(asset_id)
        if asset is not None and asset.collection == collection:
            self.assets[asset_id] = dataclasses.replace(
                asset, state="active", deleted_at=None
            )

    def set_pinned(self, collection: str, asset_id: str, pinned: bool) -> Asset:
        self.pins[asset_id] = pinned
        asset = self.assets.get(asset_id)
        if asset is None or asset.collection != collection:
            raise AtlasError("The asset was not found.", status=404)
        updated = dataclasses.replace(asset, pinned=pinned)
        self.assets[asset_id] = updated
        return updated

    def trash(self) -> list[TrashEntry]:
        return [
            TrashEntry(
                id=asset_id,
                state=asset.state,
                created_at=asset.created_at,
                deleted_at=asset.deleted_at,
                purged_at=asset.purged_at,
                pinned=asset.pinned,
                bytes=asset.bytes,
                content_type=asset.content_type,
                width=asset.width,
                height=asset.height,
            )
            for asset_id, asset in sorted(self.assets.items())
            if asset.state != "active"
        ]

    # --- URL helpers, mirroring the real client ----------------------------

    @staticmethod
    def original_url(asset_id: str) -> str:
        return f"/media/{asset_id}/original"

    @staticmethod
    def rendition_url(asset_id: str, max_width: int) -> str:
        return f"/media/{asset_id}/w{max_width}"


@pytest.fixture()
def atlas() -> FakeAtlas:
    return FakeAtlas()


@pytest.fixture()
def client(atlas: FakeAtlas, tmp_path: Path):
    app = create_app(atlas=atlas, database=tmp_path / "album.sqlite3")
    app.config.update(TESTING=True)
    return app.test_client()


def png_bytes(seed: object = "") -> bytes:
    """Unique bytes per upload: Atlas deduplicates identical ones by design."""
    return PNG_BYTES + str(seed).encode()


def upload_png(client, name: str = "holiday.png", seed: object = "") -> str:
    response = client.post(
        "/upload",
        data={"photos": (io.BytesIO(png_bytes(seed or name)), name)},
        content_type="multipart/form-data",
    )
    assert response.status_code == 200, response.get_data(as_text=True)
    return response.get_data(as_text=True)


def upload_document(client, name: str = "menu.pdf", seed: object = "") -> str:
    response = client.post(
        "/upload",
        data={
            "photos": (
                io.BytesIO(b"%PDF-1.7\n" + str(seed or name).encode()),
                name,
            )
        },
        content_type="multipart/form-data",
    )
    assert response.status_code == 200, response.get_data(as_text=True)
    return response.get_data(as_text=True)


def test_upload_stores_the_asset_and_its_widths(client, atlas):
    page = upload_png(client)

    asset_id = next(iter(atlas.assets))
    assert asset_id in page
    assert "renditions: 320, 640, 960, 1280" in page

    grid = client.get("/")
    body = grid.get_data(as_text=True)
    # The grid uses the widths Atlas reported, not ones it invented.
    assert f"/media/{asset_id}/w640 640w" in body
    assert f"/media/{asset_id}/w960 960w" in body
    assert "w1280w" not in body


def test_a_file_that_is_not_raw_is_not_advised_to_be_converted(client, atlas):
    """The old message told every rejected file to be converted from raw.

    A text file has nothing to do with a camera, so being told to convert it
    told the person nothing at all.
    """
    response = client.post(
        "/upload",
        data={"photos": (io.BytesIO(b"notes"), "notes.txt")},
        content_type="multipart/form-data",
    )
    body = response.get_data(as_text=True)

    assert "does not know what to do with this file type" in body
    assert "raw" not in body.lower().replace("already", ""), (
        "a text file should not be told about camera raw"
    )
    assert len(atlas.assets) == 0


def test_the_album_uploads_the_formats_atlas_now_accepts(client, atlas):
    """A file Atlas produces should be uploadable, and a phone photo too.

    `.heif` is deliberately not here: it is a container family rather than a
    format, so the album will not guess what is inside it.
    """
    for extension, expected_kind in [
        ("avif", "image"),
        ("jxl", "image"),
        ("heic", "image"),
    ]:
        response = client.post(
            "/upload",
            data={
                "photos": (
                    io.BytesIO(png_bytes(extension)),
                    f"photo.{extension}",
                )
            },
            content_type="multipart/form-data",
        )
        assert response.status_code == 200, extension
        body = response.get_data(as_text=True)
        assert "result-failed" not in body, f"{extension} was refused"

    kinds = {asset.collection for asset in atlas.assets.values()}
    assert kinds == {"images"}, "all three went to the images collection"
    assert len(atlas.assets) == 3


def test_bulk_upload_reports_each_file_separately(client, atlas):
    response = client.post(
        "/upload",
        data={
            "photos": [
                (io.BytesIO(png_bytes("one")), "one.png"),
                (io.BytesIO(b"II*\0raw"), "two.cr2"),
                (io.BytesIO(png_bytes("three")), "three.png"),
            ]
        },
        content_type="multipart/form-data",
    )
    body = response.get_data(as_text=True)
    assert "one.png" in body and "three.png" in body
    assert "camera raw file" in body
    assert len(atlas.assets) == 2


def test_a_failed_upload_does_not_stop_the_rest(client, atlas):
    atlas.fail_next_upload = "The file contents do not match the declared media type."

    response = client.post(
        "/upload",
        data={
            "photos": [
                (io.BytesIO(png_bytes("broken")), "broken.png"),
                (io.BytesIO(png_bytes("fine")), "fine.png"),
            ]
        },
        content_type="multipart/form-data",
    )
    body = response.get_data(as_text=True)
    assert "do not match the declared media type" in body
    assert "1 of 2 uploaded" in body


def test_a_small_photo_renders_its_real_width_in_the_srcset(client, atlas):
    """A photo narrower than the largest profile still gets a correct srcset.

    Atlas caps every rendition at the source width, so `max_width` is what was
    asked for and `width` is what came out. The descriptor has to be the real
    one, or the browser reserves space for an image that does not exist. The URL
    keeps pointing at the profile Atlas stored it under, because that is the URL
    Atlas serves.
    """
    atlas.source_width = 500

    upload_png(client)
    asset_id = next(iter(atlas.assets))
    stored = atlas.assets[asset_id].renditions
    assert [r.max_width for r in stored] == [320, 640, 960, 1280]
    assert [r.width for r in stored] == [320, 500, 500, 500]

    grid = client.get("/").get_data(as_text=True)
    assert f"/media/{asset_id}/w640 500w" in grid
    assert " 640w" not in grid, "no descriptor claims a width that was never produced"

    photo_id = _only_photo_id(client, atlas)
    detail = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert f"/media/{asset_id}/w960 500w" in detail
    assert f"/media/{asset_id}/w1280 500w" in detail


def test_the_uploaded_name_travels_to_atlas(client, atlas):
    upload_png(client, "Unsere Familie.png", seed="family")
    sent = [name for name in atlas.file_names if name]
    assert sent == ["Unsere Familie.png"], sent

    # The client percent-encodes, because Python's HTTP layer would raise on a
    # non-Latin-1 header value.
    from urllib.parse import quote

    assert quote("Unsere Familie.png", safe="") == "Unsere%20Familie.png"


def test_uploading_identical_bytes_points_at_the_existing_photo(client, atlas):
    """Atlas deduplicates; the album keeps one row per asset and says so."""
    upload_png(client, "beach.png", seed="beach")
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))
    client.post(
        f"/photos/{photo_id}",
        data={"description": "Sunset at the pier", "alt_text": "", "tags": "summer"},
    )

    again = upload_png(client, "beach-again.png", seed="beach")
    assert "Already in the album" in again, again
    assert asset_id in again
    assert f"/photos/{photo_id}" in again, "the existing photo is linked"

    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        photos = album_db.list_photos(connection)
    finally:
        connection.close()
    assert len(photos) == 1, "one album row, not two rows for one asset"


def test_a_repeated_upload_of_a_document_reports_the_existing_photo(client, atlas):
    upload_document(client, seed="menu")
    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        photo_id = album_db.list_photos(connection)[0].id
    finally:
        connection.close()

    page = upload_document(client, seed="menu")
    assert "Already in the album" in page
    assert f"/photos/{photo_id}" in page


def test_a_picture_can_be_stored_as_a_document_instead_of_being_converted(
    client, atlas
):
    """The reason Atlas has two collections: keep this file exactly as it is.

    Atlas's routes are typed, so the album says which collection it means by
    ticking a box. Nothing else changes, and the album remembers the answer
    because later calls need it.
    """
    response = client.post(
        "/upload",
        data={
            "photos": (io.BytesIO(png_bytes("diagram")), "diagram.png"),
            "as_document": "yes",
        },
        content_type="multipart/form-data",
    )
    assert response.status_code == 200

    stored = atlas.assets[next(iter(atlas.assets))]
    assert stored.collection == "documents", "stored whole, not converted"
    assert stored.kind == "document", "Atlas's own word for it"
    assert stored.renditions == (), "no renditions for a document"

    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        photos = album_db.list_photos(connection)
    finally:
        connection.close()
    assert len(photos) == 1
    assert photos[0].collection == "documents", "and remembered for later calls"

    # The album still renders it, and still asks Atlas what it can serve.
    page = client.get(f"/photos/{photos[0].id}")
    assert page.status_code == 200
    assert "No image to show" not in page.get_data(as_text=True)


def test_the_album_remembers_which_collection_each_asset_is_in(client, atlas):
    upload_png(client)
    upload_document(client)

    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        collections = {
            photo.content_type: photo.collection
            for photo in album_db.list_photos(connection)
        }
    finally:
        connection.close()

    assert collections["image/png"] == "images"
    assert collections["application/pdf"] == "documents"


def test_metadata_editing_never_touches_atlas(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    deleted_before = list(atlas.deleted)
    pins_before = dict(atlas.pins)

    response = client.post(
        f"/photos/{photo_id}",
        data={
            "description": "The lake in August",
            "alt_text": "A calm lake with a wooden jetty",
            "tags": "Summer, lake, summer",
        },
    )
    assert response.status_code == 302

    page = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert "The lake in August" in page
    assert "A calm lake with a wooden jetty" in page
    assert "summer, lake" in page  # deduplicated and lowercased
    assert atlas.deleted == deleted_before
    assert atlas.pins == pins_before


def test_published_posts_block_a_delete(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))

    client.post("/posts", data={"title": "Grandma at 70"})
    client.post(f"/posts/1/photos/{photo_id}")
    client.post("/posts/1/publish", data={"publish": "yes"})

    # Attaching the photo pinned the asset in Atlas.
    assert atlas.pins == {asset_id: True}

    blocked = client.post(f"/photos/{photo_id}/delete")
    assert blocked.status_code == 409
    assert "Grandma at 70" in blocked.get_data(as_text=True)
    assert atlas.deleted == []
    assert client.get("/").status_code == 200

    forced = client.post(f"/photos/{photo_id}/delete", data={"confirm": "yes"})
    assert forced.status_code == 302
    assert atlas.deleted == [asset_id]
    assert forced.headers["Location"] == "/"


def test_deleting_an_unused_photo_is_allowed(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    assert client.post(f"/photos/{photo_id}/delete").status_code == 302
    assert len(atlas.deleted) == 1
    assert "Nothing here yet" in client.get("/").get_data(as_text=True)


def test_a_live_photo_renders_an_image_and_no_empty_state(client, atlas):
    upload_png(client)
    asset_id = next(iter(atlas.assets))

    for page in (
        client.get("/"),
        client.get(f"/photos/{_only_photo_id(client, atlas)}"),
    ):
        body = page.get_data(as_text=True)
        assert f"/media/{asset_id}" in body
        assert "No image to show" not in body, (
            "a live photo must not show an empty state"
        )
        # The fallback is in the markup but stays hidden until a load fails.
        assert '<div class="missing" data-fallback hidden>' in body


def test_a_missing_asset_says_so_instead_of_showing_a_broken_image(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    # Atlas forgot about it - a purged tombstone, or a wiped database.
    atlas.assets.clear()

    detail = client.get(f"/photos/{photo_id}")
    assert detail.status_code == 200
    body = detail.get_data(as_text=True)
    assert "No image to show" in body
    assert "Atlas has no record of this asset" in body
    assert 'data-state="missing"' in body

    grid = client.get("/")
    assert grid.status_code == 200
    grid_body = grid.get_data(as_text=True)
    assert "No image to show" in grid_body
    assert "<img" not in grid_body, "an unknown asset gets an empty state, not an <img>"


def test_a_purged_asset_is_explained_as_unrecoverable(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))
    atlas.assets[asset_id] = dataclasses.replace(atlas.assets[asset_id], state="purged")

    body = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert "Purged by Atlas" in body
    assert "gone for good" in body
    assert "Restore from the trash" not in body, "purged bytes cannot be restored"


def test_an_unreachable_atlas_is_reported_without_breaking_the_page(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    def explode(asset_ids: list[str]):
        raise AtlasError("connection refused", status=503)

    atlas.states = explode  # type: ignore[method-assign]

    page = client.get(f"/photos/{photo_id}")
    assert page.status_code == 200
    body = page.get_data(as_text=True)
    assert "Atlas did not answer" in body
    assert "could not be reached" in body


def test_a_page_asks_about_every_asset_in_one_call(client, atlas):
    upload_png(client, "one.png")
    upload_png(client, "two.png")
    upload_png(client, "three.png")
    assert len(atlas.assets) == 3

    page = client.get("/")
    assert page.status_code == 200

    calls = atlas.state_calls
    assert len(calls) == 1, "one batched call per page, not one per photo"
    assert sorted(calls[0]) == sorted(atlas.assets)


def test_a_repeated_asset_costs_nothing(client, atlas):
    upload_png(client)
    asset_id = next(iter(atlas.assets))

    client.get("/")
    client.get(f"/photos/{_only_photo_id(client, atlas)}")

    assert atlas.state_calls.count([asset_id]) == 2
    assert len(atlas.state_calls) == 2, "each request asks once"


def test_state_lookups_are_chunked(client, atlas, monkeypatch):
    """A page larger than one lookup is split rather than refused."""
    import app as app_module

    monkeypatch.setattr(app_module, "STATE_CHUNK", 2)
    upload_png(client, "one.png")
    upload_png(client, "two.png")
    upload_png(client, "three.png")

    client.get("/")

    assert len(atlas.state_calls) == 2
    asked = [asset_id for call in atlas.state_calls for asset_id in call]
    assert sorted(asked) == sorted(atlas.assets)


def test_a_trashed_asset_offers_a_restore(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))

    atlas.assets[asset_id] = dataclasses.replace(
        atlas.assets[asset_id], state="deleted", deleted_at=1_700_000_100
    )

    body = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert "deleted" in body
    assert "Restore from the trash" in body

    assert client.post(f"/trash/images/{asset_id}/restore").status_code == 302
    assert atlas.restored == [asset_id]


def test_trash_page_lists_purged_tombstones(client, atlas):
    """The album deleted it; Atlas purged it. Both facts are worth showing.

    The album can still name the photo, because it kept the row. Atlas can no
    longer produce the bytes. Neither service alone could tell this story.
    """
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))
    assert client.post(f"/photos/{photo_id}/delete").status_code == 302

    atlas.assets[asset_id] = dataclasses.replace(atlas.assets[asset_id], state="purged")

    body = client.get("/trash").get_data(as_text=True)
    assert asset_id in body
    assert "Purged; the bytes are gone." in body
    assert "/restore" not in body, "a purged photo offers no way back"


def test_a_restored_photo_comes_back_to_the_album(client, atlas):
    """Deleting then restoring must not quietly lose the photo.

    The album's row carries the description, alt text, tags and folder, and no
    amount of asking Atlas could rebuild any of them. A restore that leaves the
    photo out of the album has destroyed them for good.
    """
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))

    client.post(f"/photos/{photo_id}", data={"description": "Grandma at 70"})
    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        photo = album_db.get_photo(connection, photo_id)
        assert photo is not None
        album_db.delete_photo(connection, photo_id)
    finally:
        connection.close()

    index = client.get("/").get_data(as_text=True)
    assert "Grandma at 70" not in index, "a deleted photo leaves the grid"

    assert client.post(f"/trash/images/{asset_id}/restore").status_code == 302

    index = client.get("/").get_data(as_text=True)
    assert "Grandma at 70" in index, "and comes back whole when restored"

    connection = album_db.connect(client.application.config["DATABASE"])
    try:
        photo = album_db.get_photo(connection, photo_id)
        assert photo is not None and not photo.is_deleted
        assert photo.description == "Grandma at 70", "with what only the album knew"
        assert photo.collection == "images", "and the collection it lives in"
    finally:
        connection.close()


def test_a_deleted_photo_still_has_a_page_that_explains_itself(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))
    client.post(f"/photos/{photo_id}/delete")

    body = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert body  # not a 404
    assert "You deleted this photo from the album" in body
    assert "Restore from the trash" in body, "the way out is on the page itself"

    assert client.post(f"/trash/images/{asset_id}/restore").status_code == 302


def test_the_trash_lists_only_what_the_album_deleted(client, atlas):
    """Atlas's trash also holds assets uploaded elsewhere; they are not ours.

    Restoring one of those could not put it back into the album, because the
    album never had a row for it. Showing it would be offering an undo that
    cannot work.
    """
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    client.post(f"/photos/{photo_id}/delete")

    # Something in Atlas's trash that the album has no record of, as if a CLI
    # had uploaded and removed it.
    orphan = Asset(
        id="zzzzzzzzzzzzzzzzzz",
        kind="image",
        created_at=1_700_000_000,
        deduplicated=False,
        state="deleted",
        pinned=False,
        deleted_at=1_700_000_100,
        purged_at=None,
        content_type="image/png",
        bytes=2048,
        sha256="cafe",
        width=100,
        height=80,
        original_url="/media/zzzzzzzzzzzzzzzzzz/original",
        renditions=(),
    )
    atlas.assets[orphan.id] = orphan

    body = client.get("/trash").get_data(as_text=True)
    assert orphan.id not in body, "an asset the album never had is not ours to list"
    assert next(iter(atlas.assets)) != orphan.id


def test_atlas_failures_are_reported_not_swallowed(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    def explode(collection: str, asset_id: str) -> None:
        raise AtlasError(
            "The asset is in the trash but its files were not moved.", status=500
        )

    atlas.delete = explode  # type: ignore[method-assign]
    response = client.post(f"/photos/{photo_id}/delete", follow_redirects=True)
    assert "refused to delete" in response.get_data(as_text=True)
    assert client.get("/").status_code == 200


def _only_photo_id(client, atlas: FakeAtlas) -> int:
    """The one photo the fake Atlas accepted."""
    assert len(atlas.assets) == 1, "expected exactly one uploaded asset"
    database = client.application.config["DATABASE"]
    connection = album_db.connect(database)
    try:
        photos = album_db.list_photos(connection)
    finally:
        connection.close()
    assert len(photos) == 1
    return photos[0].id

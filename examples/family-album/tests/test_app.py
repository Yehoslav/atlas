"""The album's own tests.

Atlas is replaced by an in-memory fake that speaks the same small interface, so
these tests exercise the integration contract - what the album stores, what it
sends, and what it does when an asset is missing - without a running Atlas.
"""

from __future__ import annotations

import io
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import album_db
from app import create_app
from atlas_client import Asset, AtlasError, Rendition, TrashEntry

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
        # Source width of the next upload. Atlas caps renditions at the source,
        # so a small photo stores narrower images than its profiles ask for.
        self.source_width = 1600

    def _make(self, state: str = "active", pinned: bool = False) -> Asset:
        self.next_id += 1
        asset_id = f"{self.next_id:018d}"
        source = self.source_width
        asset = Asset(
            id=asset_id,
            created_at=1_700_000_000,
            state=state,
            pinned=pinned,
            deleted_at=None if state == "active" else 1_700_000_100,
            purged_at=None,
            content_type="image/png",
            bytes=2048,
            sha256="deadbeef",
            width=source,
            height=int(source * 3 / 4),
            original_url=f"/media/{asset_id}/original",
            renditions=tuple(
                Rendition(
                    max_width=profile,
                    width=min(profile, source),
                    height=int(min(profile, source) * 3 / 4),
                    formats=("webp", "png"),
                )
                for profile in (320, 640, 960, 1280)
            ),
        )
        self.assets[asset_id] = asset
        return asset

    # --- the interface app.py uses -----------------------------------------

    def upload(self, stream, content_type: str) -> Asset:
        if self.fail_next_upload:
            message, self.fail_next_upload = self.fail_next_upload, None
            raise AtlasError(message, status=415)
        return self._make()

    def get_asset(self, asset_id: str) -> Asset | None:
        return self.assets.get(asset_id)

    def delete(self, asset_id: str) -> None:
        self.deleted.append(asset_id)
        self.assets.pop(asset_id, None)

    def restore(self, asset_id: str) -> None:
        self.restored.append(asset_id)

    def set_pinned(self, asset_id: str, pinned: bool) -> Asset:
        self.pins[asset_id] = pinned
        asset = self.assets.get(asset_id)
        if asset is None:
            raise AtlasError("The asset was not found.", status=404)
        updated = Asset(**{**asset.__dict__, "pinned": pinned})
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


def upload_png(client, name: str = "holiday.png") -> str:
    response = client.post(
        "/upload",
        data={"photos": (io.BytesIO(PNG_BYTES), name)},
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


def test_bulk_upload_reports_each_file_separately(client, atlas):
    response = client.post(
        "/upload",
        data={
            "photos": [
                (io.BytesIO(PNG_BYTES), "one.png"),
                (io.BytesIO(b"II*\0raw"), "two.cr2"),
                (io.BytesIO(PNG_BYTES), "three.png"),
            ]
        },
        content_type="multipart/form-data",
    )
    body = response.get_data(as_text=True)
    assert "one.png" in body and "three.png" in body
    assert "camera RAW" in body
    assert len(atlas.assets) == 2


def test_a_failed_upload_does_not_stop_the_rest(client, atlas):
    atlas.fail_next_upload = "The file contents do not match the declared media type."

    response = client.post(
        "/upload",
        data={
            "photos": [
                (io.BytesIO(PNG_BYTES), "broken.png"),
                (io.BytesIO(PNG_BYTES), "fine.png"),
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
    atlas.assets[asset_id] = Asset(
        **{**atlas.assets[asset_id].__dict__, "state": "purged"}
    )

    body = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert "Purged by Atlas" in body
    assert "gone for good" in body
    assert "Restore from the trash" not in body, "purged bytes cannot be restored"


def test_an_unreachable_atlas_is_reported_without_breaking_the_page(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    def explode(asset_id: str) -> None:
        raise AtlasError("connection refused", status=503)

    atlas.get_asset = explode  # type: ignore[method-assign]

    page = client.get(f"/photos/{photo_id}")
    assert page.status_code == 200
    body = page.get_data(as_text=True)
    assert "Atlas did not answer" in body
    assert "could not be reached" in body


def test_each_asset_is_asked_about_once_per_page(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    calls: list[str] = []
    original = atlas.get_asset

    def counting(asset_id: str):
        calls.append(asset_id)
        return original(asset_id)

    atlas.get_asset = counting  # type: ignore[method-assign]

    page = client.get(f"/photos/{photo_id}")
    assert page.status_code == 200
    assert calls == [next(iter(atlas.assets))]


def test_a_trashed_asset_offers_a_restore(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)
    asset_id = next(iter(atlas.assets))

    atlas.assets[asset_id] = Asset(
        **{
            **atlas.assets[asset_id].__dict__,
            "state": "deleted",
            "deleted_at": 1_700_000_100,
        }
    )

    body = client.get(f"/photos/{photo_id}").get_data(as_text=True)
    assert "deleted" in body
    assert "Restore from the trash" in body

    assert client.post(f"/trash/{asset_id}/restore").status_code == 302
    assert atlas.restored == [asset_id]


def test_trash_page_lists_purged_tombstones(client, atlas):
    upload_png(client)
    asset_id = next(iter(atlas.assets))
    atlas.assets[asset_id] = Asset(
        **{**atlas.assets[asset_id].__dict__, "state": "purged"}
    )

    body = client.get("/trash").get_data(as_text=True)
    assert asset_id in body
    assert "Purged; the bytes are gone." in body


def test_atlas_failures_are_reported_not_swallowed(client, atlas):
    upload_png(client)
    photo_id = _only_photo_id(client, atlas)

    def explode(asset_id: str) -> None:
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

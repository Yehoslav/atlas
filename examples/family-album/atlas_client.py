"""Server-side client for the Atlas media service.

Two rules govern this module:

1. `ATLAS_API_TOKEN` is read here and stays in this process. Nothing in it is
   ever rendered into a page, and the browser never talks to Atlas's API.
2. Atlas owns media; the album owns meaning. This client reports what Atlas
   stores (IDs, bytes, widths, lifecycle state) and nothing more.
"""

from __future__ import annotations

import os
import time
from collections.abc import Iterable
from dataclasses import dataclass
from typing import Any, BinaryIO, Protocol
from urllib.parse import quote

import requests

#: Uploads are streamed to Atlas rather than buffered, so this only bounds how
#: long a slow request may stall a worker. Image conversion is bounded by
#: Atlas, which will not convert more than one image at a time.
DEFAULT_TIMEOUT = 120

#: Atlas caps one state lookup at this many ids.
MAX_STATE_IDS = 500

#: How Atlas learns the name an uploader gave a file.
ORIGINAL_FILENAME_HEADER = "X-Original-Filename"

#: What the album is willing to try, by extension.
#:
#: Atlas accepts more than this and `GET /api/v1/formats` is the authority; the
#: table exists because the album has to pick a `Content-Type` from a filename
#: before it can ask, and an unknown extension is better refused here with a
#: sentence than sent and refused there. Camera RAW is absent deliberately:
#: those need converting before upload, whatever the file is named.
#:
#: `.heif` is absent too, though iPhones write `.heic`. HEIF is a container
#: family whose payload may be AV1 or HEVC, so which one it is has to be read
#: out of the file rather than guessed from its extension.
CONTENT_TYPES = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
    ".webp": "image/webp",
    ".avif": "image/avif",
    ".jxl": "image/jxl",
    ".heic": "image/heic",
    ".pdf": "application/pdf",
    ".doc": "application/msword",
    ".docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
}

#: Extensions that are a camera's own raw data, kept separate only so the
#: refusal can explain what to do about it.
RAW_EXTENSIONS = frozenset(
    {".cr2", ".cr3", ".nef", ".arw", ".dng", ".raf", ".orf", ".rw2", ".pef", ".srw"}
)


class AtlasError(Exception):
    """An Atlas call failed in a way worth showing to the person who pressed the button."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.message = message
        self.status = status


@dataclass(frozen=True)
class Rendition:
    """One width Atlas pre-generated. `formats` are the stored representations."""

    max_width: int
    width: int
    height: int
    formats: tuple[str, ...]


#: How Atlas reports a collection in JSON, and the URL segment that collection
#: has. The words differ on purpose: an asset is an "image", a route is under
#: "images", and the mapping lives in exactly one place.
COLLECTION_FOR_KIND = {"image": "images", "document": "documents"}


@dataclass(frozen=True)
class Asset:
    id: str
    # Which collection Atlas stored it in, as Atlas spells it: "image" or
    # "document". Typed routes need it, so it is remembered rather than derived
    # from the content type.
    kind: str
    created_at: int
    # True when this upload matched bytes Atlas already had.
    deduplicated: bool
    state: str
    pinned: bool
    deleted_at: int | None
    purged_at: int | None
    content_type: str
    bytes: int
    sha256: str
    width: int | None
    height: int | None
    original_url: str
    renditions: tuple[Rendition, ...]

    @property
    def collection(self) -> str:
        """The URL segment addressing this asset: "images" or "documents"."""
        return COLLECTION_FOR_KIND[self.kind]

    @property
    def is_live(self) -> bool:
        """Atlas will serve real bytes only for an active asset."""
        return self.state == "active"


@dataclass(frozen=True)
class AssetState:
    """What Atlas will serve for one asset: `active`, `deleted`, `purged` or `missing`."""

    id: str
    state: str
    pinned: bool

    @property
    def is_live(self) -> bool:
        return self.state == "active"


@dataclass(frozen=True)
class TrashEntry:
    id: str
    state: str
    created_at: int
    deleted_at: int | None
    purged_at: int | None
    pinned: bool
    bytes: int
    content_type: str
    width: int | None
    height: int | None

    @property
    def is_restorable(self) -> bool:
        # A purged tombstone has no bytes left, so Atlas answers 409.
        return self.state == "deleted"


def content_type_for(filename: str) -> str | None:
    _, extension = os.path.splitext(filename.lower())
    return CONTENT_TYPES.get(extension)


def _parse_asset(payload: dict[str, Any], base_url: str) -> Asset:
    original = payload["original"]
    return Asset(
        id=payload["id"],
        kind=payload["kind"],
        created_at=payload["created_at"],
        deduplicated=payload.get("deduplicated", False),
        state=payload["state"],
        pinned=payload["pinned"],
        deleted_at=payload["deleted_at"],
        purged_at=payload["purged_at"],
        content_type=original["content_type"],
        bytes=original["bytes"],
        sha256=original["sha256"],
        width=original["width"],
        height=original["height"],
        original_url=_absolute(base_url, original["url"]),
        renditions=tuple(
            Rendition(
                max_width=item["max_width"],
                width=item["width"],
                height=item["height"],
                formats=tuple(item["formats"]),
            )
            for item in payload["renditions"]
        ),
    )


def _absolute(base_url: str, path: str) -> str:
    return f"{base_url}{path}" if base_url else path


class AtlasClient:
    """Talks to one Atlas instance.

    `media_base_url` is the prefix the browser will use for `/media/` URLs. It is
    empty when the site's reverse proxy serves Atlas under the same origin as
    the album, which is the normal deployment.
    """

    def __init__(
        self,
        api_url: str,
        token: str,
        media_base_url: str | None = None,
        timeout: int = DEFAULT_TIMEOUT,
        session: requests.Session | None = None,
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self.media_base_url = (
            api_url if media_base_url is None else media_base_url
        ).rstrip("/")
        self.timeout = timeout
        self._token = token
        self._session = session or requests.Session()

    @classmethod
    def from_env(cls) -> AtlasClient:
        token = os.environ.get("ATLAS_API_TOKEN", "").strip()
        if not token:
            raise RuntimeError(
                "ATLAS_API_TOKEN must be set. It belongs to the album server, never to the browser."
            )
        return cls(
            api_url=os.environ.get("ATLAS_API_URL", "http://127.0.0.1:3000"),
            token=token,
            media_base_url=os.environ.get("ATLAS_MEDIA_URL", ""),
        )

    # --- collections --------------------------------------------------------

    @staticmethod
    def collection_for(content_type: str) -> str:
        """Which collection a content type belongs in, by Atlas's own rules."""
        return "images" if content_type.startswith("image/") else "documents"

    def collection_path(self, collection: str, asset_id: str) -> str:
        """The path for one asset's routes.

        A path, not a full URL: every call goes through `_send`, which puts
        `api_url` in front. Returning an absolute URL here would be prepended
        a second time.
        """
        return f"/api/v1/{collection}/{asset_id}"

    # --- media URLs --------------------------------------------------------

    def original_url(self, asset_id: str) -> str:
        return _absolute(self.media_base_url, f"/media/{asset_id}/original")

    def rendition_url(self, asset_id: str, max_width: int) -> str:
        return _absolute(self.media_base_url, f"/media/{asset_id}/w{max_width}")

    # --- API ---------------------------------------------------------------

    def upload(
        self,
        stream: BinaryIO,
        content_type: str,
        file_name: str | None = None,
        collection: str | None = None,
        retries: int = 1,
        retry_delay: float = 2.0,
    ) -> Asset:
        """Stream one file to Atlas. Atlas validates it; the album only names things.

        The stream is passed to requests as-is, so a 50 MiB document never sits
        in this process's memory. Returns the asset Atlas stored, which may be
        one that already existed: `POST /api/v1/assets` answers `200` instead of
        `201` for identical bytes, and the caller can tell by its own rows.

        `file_name` goes out percent-encoded because Python's HTTP layer encodes
        header values as Latin-1 and would raise on anything else.

        `collection` is "images" or "documents" and decides what Atlas will do
        with the bytes: an image uploaded as a document is stored untouched, with
        no renditions. It defaults to the obvious one for the content type.
        """
        collection = collection or self.collection_for(content_type)
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Content-Type": content_type,
        }
        if file_name:
            headers[ORIGINAL_FILENAME_HEADER] = quote(file_name, safe="")
        attempt = 0
        while True:
            try:
                response = self._send(
                    "POST", f"/api/v1/{collection}", data=stream, headers=headers
                )
            except AtlasError:
                if attempt < retries:
                    attempt += 1
                    time.sleep(retry_delay)
                    stream.seek(0)
                    continue
                raise

            if response.status_code == 503 and attempt < retries:
                # Atlas accepts two uploads but runs one conversion at a time.
                attempt += 1
                time.sleep(retry_delay)
                stream.seek(0)
                continue

            self._raise_for_status(response)
            return _parse_asset(response.json(), self.media_base_url)

    def get_asset(self, collection: str, asset_id: str) -> Asset | None:
        """Metadata for one asset, or None when Atlas has never heard of it.

        The collection is part of the call because it is part of the API: an
        asset in the other collection is not found, exactly as if it did not
        exist at all.
        """
        response = self._send(
            "GET", self.collection_path(collection, asset_id), headers=self._headers()
        )
        if response.status_code == 404:
            return None
        self._raise_for_status(response)
        return _parse_asset(response.json(), self.media_base_url)

    def list_assets(self, collection: str, query: str = "") -> dict:
        """One page of a collection, with Atlas's own filters already applied.

        Not named `list`: a method called `list` shadows the builtin inside the
        class body, and every later `list[str]` annotation in this file would
        then describe the method rather than the type.
        """
        suffix = f"?{query}" if query else ""
        response = self._send(
            "GET", f"/api/v1/{collection}{suffix}", headers=self._headers()
        )
        self._raise_for_status(response)
        return response.json()

    def summary(self) -> dict:
        """Counts and bytes per collection, for a library header."""
        response = self._send("GET", "/api/v1/summary", headers=self._headers())
        self._raise_for_status(response)
        return response.json()

    def act(self, collection: str, action: str, ids: list[str]) -> dict:
        """One action on many assets; the result says what happened to each."""
        response = self._send(
            "POST",
            f"/api/v1/{collection}/actions",
            json={"action": action, "ids": ids},
            headers=self._headers(),
        )
        self._raise_for_status(response)
        return response.json()

    def srcset(self, asset_id: str, widths: str = "320,640,960,1280") -> dict:
        """The `src`/`srcset` Atlas built, described by real output widths.

        Every consumer would otherwise have to learn that a srcset descriptor
        is the width that was produced while the URL keeps the profile.
        """
        response = self._send(
            "GET",
            f"/api/v1/images/{asset_id}/srcset?widths={widths}",
            headers=self._headers(),
        )
        self._raise_for_status(response)
        return response.json()

    def states(self, asset_ids: list[str]) -> dict[str, AssetState]:
        """Lifecycle state for ids the caller already knows, in one call.

        This is the call that makes a gallery page affordable: without it a
        consumer has to ask about every asset one at a time. Ids Atlas has never
        heard of come back as `missing` rather than being absent, because telling
        "never existed" apart from "deleted" is the reason to ask.
        """
        if not asset_ids:
            return {}
        if len(asset_ids) > MAX_STATE_IDS:
            raise ValueError(f"at most {MAX_STATE_IDS} ids per state lookup")

        response = self._send(
            "POST",
            "/api/v1/assets/state",
            json={"ids": asset_ids},
            headers=self._headers(),
        )
        self._raise_for_status(response)
        return {
            entry["id"]: AssetState(
                id=entry["id"], state=entry["state"], pinned=entry["pinned"]
            )
            for entry in response.json()["assets"]
        }

    def delete(self, collection: str, asset_id: str) -> None:
        """Move an asset to Atlas's trash. The bytes are still restorable there."""
        self._raise_for_status(
            self._send(
                "DELETE",
                self.collection_path(collection, asset_id),
                headers=self._headers(),
            )
        )

    def restore(self, collection: str, asset_id: str) -> None:
        self._raise_for_status(
            self._send(
                "POST",
                f"{self.collection_path(collection, asset_id)}/restore",
                headers=self._headers(),
            )
        )

    def set_pinned(self, collection: str, asset_id: str, pinned: bool) -> Asset:
        """Tell Atlas a published post still uses this asset, so purge skips it.

        This is the only signal Atlas ever gets about consumer content, which is
        why it is a single boolean with no opinions attached.
        """
        response = self._send(
            "PUT",
            f"{self.collection_path(collection, asset_id)}/pin",
            json={"pinned": pinned},
            headers=self._headers(),
        )
        self._raise_for_status(response)
        return _parse_asset(response.json(), self.media_base_url)

    def trash(self) -> list[TrashEntry]:
        """Everything in Atlas's trash, across both collections.

        The album does not use this: it lists its own deletions instead, because
        an asset deleted by a command line was never in the album and
        restoring it could not put it back into the album's pages. A dashboard
        for the whole instance is what needs this, and it is the next example.
        """
        response = self._send("GET", "/api/v1/trash", headers=self._headers())
        self._raise_for_status(response)
        return [
            TrashEntry(
                id=entry["id"],
                state=entry["state"],
                created_at=entry["created_at"],
                deleted_at=entry["deleted_at"],
                purged_at=entry["purged_at"],
                pinned=entry["pinned"],
                bytes=entry["bytes"],
                content_type=entry["content_type"],
                width=entry["width"],
                height=entry["height"],
            )
            for entry in response.json()["assets"]
        ]

    # --- plumbing ----------------------------------------------------------

    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self._token}"}

    def _send(self, method: str, path: str, **kwargs: Any) -> requests.Response:
        """One API call, with every transport failure named.

        A stopped Atlas is an expected state for a page, not an exception: the
        album has to be able to say "Atlas is unreachable" instead of returning
        a 500.
        """
        try:
            return self._session.request(
                method,
                f"{self.api_url}{path}",
                timeout=self.timeout,
                **kwargs,
            )
        except requests.RequestException as error:
            # The message is read by whoever is looking at the page, so it stays
            # short; the chained cause keeps the detail for a log.
            raise AtlasError(
                f"Atlas at {self.api_url} could not be reached. Is the service running?"
            ) from error

    @staticmethod
    def _raise_for_status(response: requests.Response) -> None:
        if response.status_code < 400:
            return
        message = f"Atlas returned {response.status_code}"
        try:
            message = response.json()["error"]["message"]
        except (ValueError, KeyError, TypeError):
            pass
        raise AtlasError(message, status=response.status_code)


def widths_available(photo: Any, wanted: Iterable[int]) -> list[int]:
    """The subset of `wanted` widths that Atlas actually generated for a photo."""
    available = {rendition.max_width for rendition in photo.renditions}
    return [width for width in wanted if width in available]


class AtlasLike(Protocol):
    """What the album actually needs from Atlas.

    `create_app` takes one of these rather than an `AtlasClient`, so the
    album's own tests can pass a fake that answers the same questions without
    being told to subclass a class it does not use. Anything the album starts
    calling shows up here as a type error, which is the point.
    """

    def upload(
        self,
        stream: BinaryIO,
        content_type: str,
        file_name: str | None = None,
        collection: str | None = None,
    ) -> Asset: ...

    def get_asset(self, collection: str, asset_id: str) -> Asset | None: ...

    def delete(self, collection: str, asset_id: str) -> None: ...

    def restore(self, collection: str, asset_id: str) -> None: ...

    def set_pinned(self, collection: str, asset_id: str, pinned: bool) -> Asset: ...

    def states(self, asset_ids: list[str]) -> dict[str, AssetState]: ...

    def original_url(self, asset_id: str) -> str: ...

    def rendition_url(self, asset_id: str, max_width: int) -> str: ...

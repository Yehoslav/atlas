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
from typing import Any, BinaryIO

import requests

#: Uploads are streamed to Atlas rather than buffered, so this only bounds how
#: long a slow request may stall a worker. Image conversion is bounded by
#: Atlas, which will not convert more than one image at a time.
DEFAULT_TIMEOUT = 120

#: Atlas serves images and documents; it does not accept camera RAW.
CONTENT_TYPES = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
    ".webp": "image/webp",
    ".pdf": "application/pdf",
    ".doc": "application/msword",
    ".docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
}


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


@dataclass(frozen=True)
class Asset:
    id: str
    created_at: int
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
    def is_live(self) -> bool:
        """Atlas will serve real bytes only for an active asset."""
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
        created_at=payload["created_at"],
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
        retries: int = 1,
        retry_delay: float = 2.0,
    ) -> Asset:
        """Stream one file to Atlas. Atlas validates it; the album only names a type.

        The stream is passed to requests as-is, so a 50 MiB document never sits
        in this process's memory.
        """
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Content-Type": content_type,
        }
        attempt = 0
        while True:
            try:
                response = self._session.post(
                    f"{self.api_url}/api/v1/assets",
                    data=stream,
                    headers=headers,
                    timeout=self.timeout,
                )
            except requests.RequestException as error:
                if attempt < retries:
                    attempt += 1
                    time.sleep(retry_delay)
                    stream.seek(0)
                    continue
                raise AtlasError(f"could not reach Atlas: {error}") from error

            if response.status_code == 503 and attempt < retries:
                # Atlas accepts two uploads but runs one conversion at a time.
                attempt += 1
                time.sleep(retry_delay)
                stream.seek(0)
                continue

            self._raise_for_status(response)
            return _parse_asset(response.json(), self.media_base_url)

    def get_asset(self, asset_id: str) -> Asset | None:
        """Metadata for one asset, or None when Atlas has never heard of it."""
        response = self._session.get(
            f"{self.api_url}/api/v1/assets/{asset_id}",
            headers=self._headers(),
            timeout=self.timeout,
        )
        if response.status_code == 404:
            return None
        self._raise_for_status(response)
        return _parse_asset(response.json(), self.media_base_url)

    def delete(self, asset_id: str) -> None:
        """Move an asset to Atlas's trash. The bytes are still restorable there."""
        self._expect_empty(
            self._session.delete(
                f"{self.api_url}/api/v1/assets/{asset_id}",
                headers=self._headers(),
                timeout=self.timeout,
            )
        )

    def restore(self, asset_id: str) -> None:
        self._expect_empty(
            self._session.post(
                f"{self.api_url}/api/v1/assets/{asset_id}/restore",
                headers=self._headers(),
                timeout=self.timeout,
            )
        )

    def set_pinned(self, asset_id: str, pinned: bool) -> Asset:
        """Tell Atlas a published post still uses this asset, so purge skips it.

        This is the only signal Atlas ever gets about consumer content, which is
        why it is a single boolean with no opinions attached.
        """
        response = self._session.put(
            f"{self.api_url}/api/v1/assets/{asset_id}/pin",
            json={"pinned": pinned},
            headers=self._headers(),
            timeout=self.timeout,
        )
        self._raise_for_status(response)
        return _parse_asset(response.json(), self.media_base_url)

    def trash(self) -> list[TrashEntry]:
        response = self._session.get(
            f"{self.api_url}/api/v1/trash",
            headers=self._headers(),
            timeout=self.timeout,
        )
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

    @staticmethod
    def _expect_empty(response: requests.Response) -> None:
        AtlasClient._raise_for_status(response)

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

"""The Atlas client, tested without a running Atlas.

A stopped Atlas is a normal state for a page, not a crash: every transport
failure has to arrive as an AtlasError, or the album returns a 500 instead of
an explanation.
"""

from __future__ import annotations

import io
import sys
from pathlib import Path

import pytest
import requests

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from atlas_client import AtlasClient, AtlasError, Rendition, content_type_for

UPLOAD_RESPONSE = {
    "id": "aaaaaaaaaaaaaaaaaaaa",
    "kind": "image",
    "created_at": 1_700_000_000,
    "state": "active",
    "deleted_at": None,
    "purged_at": None,
    "pinned": False,
    "original": {
        "url": "/media/aaaaaaaaaaaaaaaaaaaa/original",
        "content_type": "image/png",
        "bytes": 2048,
        "sha256": "deadbeef",
        "width": 1600,
        "height": 1200,
    },
    "renditions": [
        {
            "max_width": 320,
            "width": 320,
            "height": 240,
            "url": "/media/aaaaaaaaaaaaaaaaaaaa/w320",
            "formats": ["avif", "png"],
        }
    ],
}


class ScriptedSession:
    """A requests session that replays one response and records the calls."""

    def __init__(
        self, response: requests.Response | None = None, error: Exception | None = None
    ):
        self.response = response
        self.error = error
        self.calls: list[tuple[str, str]] = []
        self.methods: list[str] = []
        self.paths: list[str] = []
        self.headers_seen: dict = {}
        self.bodies: list = []

    def request(self, method: str, url: str, **kwargs) -> requests.Response:
        self.calls.append((method, url))
        self.methods.append(method)
        self.paths.append("/" + url.split("://", 1)[-1].split("/", 1)[-1])
        self.headers_seen = kwargs.get("headers") or {}
        self.bodies.append(kwargs.get("json"))
        if self.error:
            raise self.error
        assert self.response is not None
        return self.response


def response(status: int, payload: dict | None = None) -> requests.Response:
    result = requests.Response()
    result.status_code = status
    result._content = (
        b"{}" if payload is None else __import__("json").dumps(payload).encode()
    )
    return result


def client_with(session) -> AtlasClient:
    return AtlasClient(
        api_url="http://atlas.invalid:3000",
        token="secret",
        session=session,
    )


def test_a_refused_connection_becomes_an_atlas_error():
    session = ScriptedSession(error=requests.ConnectionError("connection refused"))

    with pytest.raises(AtlasError) as failure:
        client_with(session).get_asset("images", "aaaaaaaaaaaaaaaaaaaa")

    assert "could not be reached" in failure.value.message


@pytest.mark.parametrize(
    "call", ["get_asset", "delete", "restore", "trash", "set_pinned"]
)
def test_every_call_reports_an_unreachable_atlas(call):
    session = ScriptedSession(error=requests.ConnectionError("connection refused"))
    client = client_with(session)
    asset_id = "aaaaaaaaaaaaaaaaaaaa"

    with pytest.raises(AtlasError):
        if call == "get_asset":
            client.get_asset("images", asset_id)
        elif call == "delete":
            client.delete("images", asset_id)
        elif call == "restore":
            client.restore("images", asset_id)
        elif call == "trash":
            client.trash()
        else:
            client.set_pinned("images", asset_id, True)


def test_an_atlas_error_message_is_preferred_over_the_status():
    body = {
        "error": {"code": "rendition_not_found", "message": "No rendition that narrow."}
    }

    with pytest.raises(AtlasError) as failure:
        client_with(ScriptedSession(response=response(404, body))).set_pinned(
            "images", "aaaaaaaaaaaaaaaaaaaa", True
        )

    assert failure.value.message == "No rendition that narrow."
    assert failure.value.status == 404


def test_an_unknown_asset_is_none_rather_than_an_error():
    client = client_with(ScriptedSession(response=response(404)))
    assert client.get_asset("images", "aaaaaaaaaaaaaaaaaaaa") is None


def test_an_upload_response_becomes_an_asset():
    session = ScriptedSession(response=response(201, UPLOAD_RESPONSE))

    asset = client_with(session).upload(io.BytesIO(b"bytes"), "image/png")

    assert asset.id == "aaaaaaaaaaaaaaaaaaaa"
    assert asset.is_live
    assert asset.renditions == (
        Rendition(max_width=320, width=320, height=240, formats=("avif", "png")),
    )
    method, url = session.calls[0]
    assert (method, url) == ("POST", "http://atlas.invalid:3000/api/v1/images")
    assert asset.kind == "image"
    assert asset.collection == "images", "the route segment, not the JSON word"
    assert asset.deduplicated is False


def test_an_upload_sends_the_name_percent_encoded():
    session = ScriptedSession(response=response(201, UPLOAD_RESPONSE))

    client_with(session).upload(io.BytesIO(b"bytes"), "image/png", "Unsere Familie.png")

    assert session.headers_seen["X-Original-Filename"] == "Unsere%20Familie.png"
    assert session.headers_seen["Content-Type"] == "image/png"


def test_an_upload_can_choose_the_documents_collection():
    session = ScriptedSession(response=response(201, UPLOAD_RESPONSE))

    client_with(session).upload(
        io.BytesIO(b"bytes"), "image/png", collection="documents"
    )

    assert session.paths == ["/api/v1/documents"], "a picture stored as a document"


ASSET_ID = "aaaaaaaaaaaaaaaaaaaa"


def test_every_call_about_one_asset_addresses_it_by_path():
    """A path, not a URL, because `_send` puts the host in front.

    These four calls were never exercised against a transport before, so a
    helper that returned a full URL made every one of them send
    `http://hosthttp://host/...`, which fails only at the socket.
    """
    session = ScriptedSession(response=response(200, UPLOAD_RESPONSE))
    client = client_with(session)

    client.get_asset("images", ASSET_ID)
    client.delete("documents", ASSET_ID)
    client.restore("documents", ASSET_ID)
    client.set_pinned("documents", ASSET_ID, True)

    assert session.calls == [
        ("GET", f"http://atlas.invalid:3000/api/v1/images/{ASSET_ID}"),
        ("DELETE", f"http://atlas.invalid:3000/api/v1/documents/{ASSET_ID}"),
        ("POST", f"http://atlas.invalid:3000/api/v1/documents/{ASSET_ID}/restore"),
        ("PUT", f"http://atlas.invalid:3000/api/v1/documents/{ASSET_ID}/pin"),
    ]
    assert session.bodies[-1] == {"pinned": True}


def test_the_rest_of_the_client_addresses_the_api_consistently():
    session = ScriptedSession(response=response(200, UPLOAD_RESPONSE))
    client = client_with(session)

    client.list_assets("documents", "state=deleted")
    client.summary()
    client.act("documents", "delete", [ASSET_ID])
    client.srcset(ASSET_ID)

    assert session.paths == [
        "/api/v1/documents?state=deleted",
        "/api/v1/summary",
        "/api/v1/documents/actions",
        f"/api/v1/images/{ASSET_ID}/srcset?widths=320,640,960,1280",
    ]
    assert session.methods == ["GET", "GET", "POST", "GET"]


def test_the_batch_state_call_is_the_one_untyped_path():
    """It has to be: ids come from anywhere and may be of either collection."""
    session = ScriptedSession(
        response=response(
            200, {"assets": [{"id": ASSET_ID, "state": "active", "pinned": True}]}
        )
    )

    states = client_with(session).states([ASSET_ID])

    assert session.paths == ["/api/v1/assets/state"]
    assert states[ASSET_ID].pinned is True


def test_the_default_collection_follows_the_content_type():
    client = client_with(ScriptedSession())
    assert client.collection_for("image/png") == "images"
    assert client.collection_for("image/jpeg") == "images"
    assert client.collection_for("application/pdf") == "documents"


def test_a_listing_and_a_bulk_action_address_one_collection():
    session = ScriptedSession(response=response(200, {"assets": [], "total": 0}))
    client = client_with(session)

    client.list_assets("documents", "limit=10")
    client.act("images", "delete", ["aaaaaaaaaaaaaaaaaaaa"])
    client.summary()
    client.srcset("aaaaaaaaaaaaaaaaaaaa")

    assert session.paths == [
        "/api/v1/documents?limit=10",
        "/api/v1/images/actions",
        "/api/v1/summary",
        "/api/v1/images/aaaaaaaaaaaaaaaaaaaa/srcset?widths=320,640,960,1280",
    ]


def test_an_upload_without_a_name_sends_no_name_header():
    session = ScriptedSession(response=response(201, UPLOAD_RESPONSE))

    client_with(session).upload(io.BytesIO(b"bytes"), "image/png")

    assert "X-Original-Filename" not in session.headers_seen


def test_a_state_lookup_posts_every_id():
    session = ScriptedSession(
        response=response(
            200,
            {
                "assets": [
                    {"id": "aaaaaaaaaaaaaaaaaaaa", "state": "active", "pinned": True},
                    {"id": "bbbbbbbbbbbbbbbbbb", "state": "missing", "pinned": False},
                ]
            },
        )
    )

    found = client_with(session).states(["aaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbb"])

    assert session.methods == ["POST"]
    assert session.paths == ["/api/v1/assets/state"]
    assert found["aaaaaaaaaaaaaaaaaaaa"].is_live
    assert found["bbbbbbbbbbbbbbbbbb"].state == "missing"


def test_a_state_lookup_refuses_more_ids_than_atlas_accepts():
    client = client_with(ScriptedSession())
    with pytest.raises(ValueError):
        client.states(["a" * 18] * 501)


def test_media_urls_are_absolute_only_when_a_media_base_is_configured():
    same_origin = AtlasClient("http://atlas.invalid:3000", "secret", media_base_url="")
    assert same_origin.rendition_url("abc", 640) == "/media/abc/w640"

    proxied = AtlasClient(
        "http://atlas.invalid:3000", "secret", media_base_url="http://127.0.0.1:3000"
    )
    assert proxied.rendition_url("abc", 640) == "http://127.0.0.1:3000/media/abc/w640"


def test_only_supported_content_types_are_named():
    assert content_type_for("holiday.JPG") == "image/jpeg"
    assert content_type_for("report.pdf") == "application/pdf"
    assert content_type_for("raw.cr2") is None
    assert content_type_for("script.sh") is None

#!/usr/bin/env python3
"""End-to-end smoke test against a real Atlas binary.

Starts the server on a throwaway data directory, checks the HTTP surface over a
real socket with real conversions and real bytes, optionally points the example
consumer's own client at it, and always shuts the server down — including on
Ctrl-C or a failed check.

    nix develop --command python3 scripts/smoke.py

Why Python and not shell: this script's hard part is process management, which
is what shell is worst at. `Popen` owns the pid, so nothing ever pattern-matches
a command line (a `pkill -f` matches the shell running it); `start_new_session`
detaches the server so a Ctrl-C here cannot take it down mid-test; `wait(timeout)`
is a real bounded wait; and `try/finally` guarantees the server is stopped. A
stale server holding the port is the failure mode that produces phantom errors,
so the port is checked before starting and the pid is verified after.

Exit codes: 0 all checks passed, 1 a check failed, 2 the harness itself failed.
"""

from __future__ import annotations

import argparse
import html
import http.cookiejar
import json
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib
from pathlib import Path
from typing import Any, Final, Self

REPO = Path(__file__).resolve().parents[1]
DEFAULT_PORT = 3200
TOKEN = "smoke-token-abc"
HEALTH_TIMEOUT = 15.0

#: Sentinel for "use this client's token", distinct from "send no token".
USE_DEFAULT_TOKEN: Final = object()


class HarnessError(Exception):
    """The harness could not run the test at all, as opposed to a failed check."""


class Server:
    """One Atlas process, started and stopped by pid and nothing else."""

    def __init__(
        self, binary: Path, port: int, data_dir: Path, token: str, log: Path
    ) -> None:
        self.binary = binary
        self.port = port
        self.data_dir = data_dir
        self.token = token
        self.log = log
        self.process: subprocess.Popen[bytes] | None = None
        self.pid: int | None = None

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def check_port_free(self) -> None:
        """Refuse to run against something else's server.

        A leftover process answering on this port would silently pass some
        checks and fail others for reasons that have nothing to do with Atlas.
        """
        probe = socket.socket()
        probe.settimeout(0.5)
        try:
            if probe.connect_ex(("127.0.0.1", self.port)) == 0:
                raise HarnessError(
                    f"port {self.port} is already in use; stop that server first "
                    "(or pass --port with a free one)"
                )
        finally:
            probe.close()

    def start(self) -> None:
        if not self.binary.is_file():
            raise HarnessError(
                f"no binary at {self.binary}\n"
                "build it first: nix develop --command cargo build"
            )
        self.data_dir.mkdir(parents=True, exist_ok=True)
        environment = dict(os.environ)
        environment.update(
            ATLAS_API_TOKEN=self.token,
            ATLAS_DATA_DIR=str(self.data_dir),
            ATLAS_LISTEN_ADDR=f"127.0.0.1:{self.port}",
        )
        handle = self.log.open("wb")
        try:
            self.process = subprocess.Popen(
                [str(self.binary)],
                env=environment,
                stdout=handle,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                start_new_session=True,  # its own process group: not our signal target
            )
            self.pid = self.process.pid
        finally:
            handle.close()

        deadline = time.monotonic() + HEALTH_TIMEOUT
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise HarnessError(
                    f"server exited immediately ({self.process.returncode}):\n{self.tail_log()}"
                )
            try:
                with urllib.request.urlopen(
                    f"{self.base_url}/healthz", timeout=2
                ) as response:
                    if response.status == 200:
                        return
            except (urllib.error.URLError, OSError):
                time.sleep(0.1)
        self.stop()
        raise HarnessError(
            f"server was not healthy within {HEALTH_TIMEOUT:.0f}s:\n{self.tail_log()}"
        )

    def stop(self) -> None:
        """Terminate, then insist. Never leaves a process holding the port."""
        if self.process is None:
            return
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.process = None

    def tail_log(self, lines: int = 10) -> str:
        if not self.log.exists():
            return "(no log)"
        return "\n".join(self.log.read_text(errors="replace").splitlines()[-lines:])

    def __enter__(self) -> Self:
        self.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.stop()


def decode(payload: bytes, raw: bool) -> Any:
    """JSON when the body is JSON, bytes when it is a file.

    A check that only wants the status code should not have to say `raw=True`,
    and one that wants the bytes should not get a UnicodeDecodeError from a
    PNG for forgetting to.
    """
    if raw:
        return payload
    try:
        return json.loads(payload or b"null")
    except (UnicodeDecodeError, json.JSONDecodeError):
        return payload


class Checks:
    def __init__(self, base_url: str, token: str) -> None:
        self.base_url = base_url
        self.token = token
        self.failures = 0
        self.total = 0
        self.skipped = 0

    def skip(self, name: str, why: str) -> None:
        """A check that could not run. Counted separately: never green."""
        self.skipped += 1
        print(f"SKIP  {name}: {why}", flush=True)

    def ok(self, name: str, got: object, want: object) -> bool:
        self.total += 1
        passed = got == want
        if not passed:
            self.failures += 1
        detail = f"{got!r}" + ("" if passed else f" (want {want!r})")
        print(f"{'PASS' if passed else 'FAIL'}  {name}: {detail}", flush=True)
        return passed

    def true(self, name: str, condition: bool) -> bool:
        return self.ok(name, bool(condition), True)

    def request(
        self,
        method: str,
        path: str,
        body: bytes | dict | None = None,
        headers: dict | None = None,
        token: Any = USE_DEFAULT_TOKEN,
        raw: bool = False,
    ) -> tuple[int, Any]:
        head = dict(headers or {})
        data: bytes | None = None
        if isinstance(body, (dict, list)):
            data = json.dumps(body).encode()
            head["Content-Type"] = "application/json"
        elif isinstance(body, str):
            data = body.encode()
        elif isinstance(body, bytes):
            data = body
        bearer = self.token if token is USE_DEFAULT_TOKEN else token
        if bearer is not None:
            head["Authorization"] = f"Bearer {bearer}"
        request = urllib.request.Request(
            self.base_url + path, data=data, headers=head, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=90) as response:
                return response.status, decode(response.read(), raw)
        except urllib.error.HTTPError as error:
            return error.code, decode(error.read(), raw)


def png_bytes(width: int = 1400, height: int = 900) -> bytes:
    """A real PNG, generated so the test needs no fixture files."""

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return (
            struct.pack(">I", len(payload))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    rows = b"".join(
        b"\x00" + bytes([(x * 7 + y * 3) % 256 for x in range(width * 3)])
        for y in range(height)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows))
        + chunk(b"IEND", b"")
    )


def image_bytes(magick_format: str, width: int = 120, height: int = 90) -> bytes:
    """Real bytes in a real format, generated so the checks need no fixtures.

    ImageMagick makes these, the same way Atlas would read them back.
    """
    generated = subprocess.run(
        ["magick", "-size", f"{width}x{height}", "xc:red", f"{magick_format}:-"],
        capture_output=True,
        check=True,
    )
    return generated.stdout


def heic_bytes(width: int = 120, height: int = 90) -> bytes:
    return image_bytes("HEIC", width, height)


def avif_bytes(width: int = 120, height: int = 90) -> bytes:
    return image_bytes("AVIF", width, height)


PDF = b"%PDF-1.4\n% atlas smoke test\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n"
IMAGE_HEADERS = {"Content-Type": "image/png", "X-Original-Filename": "big.png"}


def check_api(checks: Checks) -> None:
    """The HTTP contract, over a socket, against a real ImageMagick."""
    print("\n== uploads ==", flush=True)
    status, image = checks.request("POST", "/api/v1/images", png_bytes(), IMAGE_HEADERS)
    checks.ok("image upload", status, 201)
    checks.ok("  kind", image["kind"], "image")
    checks.ok("  content type", image["original"]["content_type"], "image/png")
    checks.ok("  filename kept", image["original"]["filename"], "big.png")
    checks.ok(
        "  rendition widths",
        [r["width"] for r in image["renditions"]],
        [320, 640, 960, 1280],
    )
    image_id = image["id"]

    status, duplicate = checks.request(
        "POST", "/api/v1/images", png_bytes(), {"Content-Type": "image/png"}
    )
    checks.ok("same bytes again", status, 200)
    checks.ok("  deduplicated", duplicate.get("deduplicated"), True)
    checks.ok("  same id", duplicate["id"], image_id)

    status, as_document = checks.request(
        "POST", "/api/v1/documents", png_bytes(), IMAGE_HEADERS
    )
    checks.ok("same bytes as a document", status, 201)
    checks.ok("  kind", as_document["kind"], "document")
    checks.ok("  no renditions", len(as_document["renditions"]), 0)
    checks.true("  a separate asset", as_document["id"] != image_id)

    status, document = checks.request(
        "POST",
        "/api/v1/documents",
        PDF,
        {"Content-Type": "application/pdf", "X-Original-Filename": "note.pdf"},
    )
    checks.ok("pdf upload", status, 201)
    document_id = document["id"]
    checks.ok(
        "raw camera format rejected",
        checks.request(
            "POST", "/api/v1/documents", b"raw", {"Content-Type": "image/x-canon-cr3"}
        )[0],
        415,
    )

    print("\n== typed routes ==", flush=True)
    checks.ok(
        "image asked for as a document",
        checks.request("GET", f"/api/v1/documents/{image_id}")[0],
        404,
    )
    checks.ok(
        "document asked for as an image",
        checks.request("GET", f"/api/v1/images/{document_id}")[0],
        404,
    )
    checks.ok(
        "delete in the wrong collection",
        checks.request("DELETE", f"/api/v1/documents/{image_id}")[0],
        404,
    )
    checks.ok(
        "  and it is untouched",
        checks.request("GET", f"/api/v1/images/{image_id}")[0],
        200,
    )

    print("\n== listing ==", flush=True)
    checks.ok("images listed", checks.request("GET", "/api/v1/images")[1]["total"], 1)
    checks.ok(
        "documents listed", checks.request("GET", "/api/v1/documents")[1]["total"], 2
    )
    checks.ok(
        "limit does not change total",
        checks.request("GET", "/api/v1/documents?limit=1")[1]["total"],
        2,
    )
    checks.ok(
        "sort by bytes",
        checks.request("GET", "/api/v1/images?sort=bytes&order=asc")[0],
        200,
    )
    checks.ok(
        "sort by filename",
        checks.request("GET", "/api/v1/documents?sort=filename")[0],
        200,
    )
    checks.ok("q matches", checks.request("GET", "/api/v1/images?q=big")[1]["total"], 1)
    checks.ok(
        "q misses", checks.request("GET", "/api/v1/images?q=absent")[1]["total"], 0
    )
    checks.ok(
        "q is not a wildcard",
        checks.request("GET", "/api/v1/images?q=%25")[1]["total"],
        0,
    )
    checks.ok(
        "mime, exact type",
        checks.request("GET", "/api/v1/images?mime=image%2Fpng")[1]["total"],
        1,
    )
    checks.ok(
        "mime, a bare star survives the wire",
        checks.request("GET", "/api/v1/images?mime=image%2F%2A")[1]["total"],
        1,
    )
    checks.ok(
        "mime, a group in documents",
        checks.request("GET", "/api/v1/documents?mime=image%2F%2A")[1]["total"],
        1,
    )
    checks.ok(
        "mime, exact in documents",
        checks.request("GET", "/api/v1/documents?mime=application%2Fpdf")[1]["total"],
        1,
    )
    checks.ok(
        "mime, an unknown type is refused",
        checks.request("GET", "/api/v1/images?mime=image%2Ftiff")[0],
        400,
    )
    status, error = checks.request("GET", "/api/v1/images?mime=application%2Fpdf")
    checks.ok("mime, impossible combination is refused", status, 400)
    checks.ok("  with the same code", error["error"]["code"], "invalid_mime")
    checks.true(
        "  and points at the formats listing",
        "/api/v1/formats" in error["error"]["message"],
    )
    checks.ok(
        "bad sort is a 400",
        checks.request("GET", "/api/v1/images?sort=largest")[0],
        400,
    )
    checks.ok(
        "bad state is a 400",
        checks.request("GET", "/api/v1/images?state=elsewhere")[0],
        400,
    )
    checks.ok(
        "no token is a 401", checks.request("GET", "/api/v1/images", token=None)[0], 401
    )

    print("\n== a consumer's own data ==", flush=True)
    # Deliberately awkward: unicode, markup, and something that only looks like
    # JSON. It has to come back exactly as it went in.
    opaque = 'Прогулки\n<em>с Alder</em> — {"looks": "like json"}'
    status, _ = checks.request(
        "PUT",
        f"/api/v1/documents/{document_id}/data",
        {"data": opaque},
    )
    checks.ok("stored", status, 200)
    status, asset = checks.request("GET", f"/api/v1/documents/{document_id}")
    checks.ok("  comes back verbatim", asset["data"], opaque)

    checks.ok(
        "  replaceable",
        checks.request(
            "PUT", f"/api/v1/documents/{document_id}/data", {"data": "shorter"}
        )[0],
        200,
    )
    checks.request("PUT", f"/api/v1/documents/{document_id}/data", {"data": None})
    _, cleared = checks.request("GET", f"/api/v1/documents/{document_id}")
    checks.true("  and clearable", "data" not in cleared)

    checks.request("PUT", f"/api/v1/documents/{document_id}/data", {"data": "keep me"})
    _, listed = checks.request("GET", "/api/v1/documents")
    entry = next(
        (a for a in listed["assets"] if a["id"] == document_id),
        {},
    )
    checks.true("  absent from listings", "data" not in entry)
    _, searched = checks.request("GET", "/api/v1/documents?q=keep%20me")
    checks.ok("  and never searched", searched["total"], 0)

    checks.ok(
        "  capped",
        checks.request(
            "PUT",
            f"/api/v1/documents/{document_id}/data",
            {"data": "x" * 70_000},
        )[0],
        400,
    )
    checks.ok(
        "  addressed by collection",
        checks.request("PUT", f"/api/v1/images/{document_id}/data", {"data": "wrong"})[
            0
        ],
        404,
    )

    print("\n== formats ==", flush=True)
    status, formats = checks.request("GET", "/api/v1/formats")
    checks.ok(
        "formats needs the token",
        checks.request("GET", "/api/v1/formats", token=None)[0],
        401,
    )
    checks.ok("formats listing", status, 200)
    advertised = formats["images"]["formats"]
    checks.ok(
        "  advertises what Atlas produces",
        sorted(advertised),
        sorted(
            [
                "image/jpeg",
                "image/png",
                "image/webp",
                "image/avif",
                "image/jxl",
                "image/heic",
                "image/heif",
            ]
        ),
    )
    checks.ok("  image group", formats["images"]["groups"], ["image/*"])
    checks.ok(
        "  images get renditions", formats["images"]["generates_renditions"], True
    )
    checks.ok(
        "  documents take everything", len(formats["documents"]["formats"]), 10
    )
    checks.ok(
        "  documents get none", formats["documents"]["generates_renditions"], False
    )

    print("\n== summary and srcset ==", flush=True)
    summary = checks.request("GET", "/api/v1/summary")[1]
    checks.ok("summary images", summary["images"]["count"], 1)
    checks.ok("summary documents", summary["documents"]["count"], 2)
    srcset = checks.request("GET", f"/api/v1/images/{image_id}/srcset")[1]
    checks.ok("srcset candidates", len(srcset["srcset"].split(", ")), 4)
    checks.true("srcset width descriptors", " 320w" in srcset["srcset"])
    checks.ok(
        "srcset on a document",
        checks.request("GET", f"/api/v1/images/{document_id}/srcset")[0],
        404,
    )

    print("\n== heif, in the spelling the desktop sends ==", flush=True)
    # HEIF has two spellings and the desktop decides which one a browser
    # sends: freedesktop.org makes `image/heif` canonical and keeps
    # `image/heic` as its alias. A file manager that answered `image/heif`
    # used to be told the format was not accepted.
    #
    # This runs after the sections that count assets, because it adds one.
    status, heic = checks.request(
        "POST",
        "/api/v1/images",
        heic_bytes(),
        {"Content-Type": "image/heif", "X-Original-Filename": "wedding.heic"},
    )
    checks.ok("heic uploaded as image/heif", status, 201)
    checks.ok("  kind", heic["kind"], "image")
    checks.ok("  stored canonically", heic["original"]["content_type"], "image/heic")
    checks.true("  it got renditions", len(heic["renditions"]) > 0)
    checks.ok(
        "the heif spelling does not admit an av1 file",
        checks.request(
            "POST", "/api/v1/images", avif_bytes(), {"Content-Type": "image/heif"}
        )[0],
        415,
    )
    checks.ok(
        "an image sequence is not a picture",
        checks.request(
            "POST",
            "/api/v1/images",
            heic_bytes(),
            {"Content-Type": "image/heif-sequence"},
        )[0],
        415,
    )

    print("\n== bulk actions ==", flush=True)
    checks.ok(
        "bulk pin",
        checks.request(
            "POST", "/api/v1/images/actions", {"action": "pin", "ids": [image_id]}
        )[0],
        200,
    )
    checks.ok(
        "  pin took effect",
        checks.request("GET", f"/api/v1/images/{image_id}")[1]["pinned"],
        True,
    )
    checks.ok(
        "bulk unpin",
        checks.request(
            "POST", "/api/v1/images/actions", {"action": "unpin", "ids": [image_id]}
        )[0],
        200,
    )
    checks.ok(
        "  pin cleared",
        checks.request("GET", f"/api/v1/images/{image_id}")[1]["pinned"],
        False,
    )
    checks.ok(
        "bulk with one unknown id",
        checks.request(
            "POST",
            "/api/v1/documents/actions",
            {"action": "pin", "ids": [document_id, "nosuchid000000000"]},
        )[0],
        400,
    )
    checks.ok(
        "  and changed nothing",
        checks.request("GET", f"/api/v1/documents/{document_id}")[1]["pinned"],
        False,
    )
    checks.ok(
        "bulk needs a known action",
        checks.request(
            "POST", "/api/v1/images/actions", {"action": "shrug", "ids": [image_id]}
        )[0],
        400,
    )

    print("\n== trash, purge states, restore ==", flush=True)
    checks.ok(
        "bulk delete",
        checks.request(
            "POST",
            "/api/v1/documents/actions",
            {"action": "delete", "ids": [document_id]},
        )[0],
        200,
    )
    checks.ok(
        "live documents", checks.request("GET", "/api/v1/documents")[1]["total"], 1
    )
    checks.ok(
        "deleted documents",
        checks.request("GET", "/api/v1/documents?state=deleted")[1]["total"],
        1,
    )
    checks.ok(
        "purged documents",
        checks.request("GET", "/api/v1/documents?state=purged")[1]["total"],
        0,
    )
    checks.ok(
        "all documents",
        checks.request("GET", "/api/v1/documents?state=all")[1]["total"],
        2,
    )
    checks.ok("trash lists it", checks.request("GET", "/api/v1/trash")[1]["total"], 1)
    checks.ok(
        "it reads as deleted",
        checks.request("GET", f"/api/v1/documents/{document_id}")[1]["state"],
        "deleted",
    )
    checks.ok(
        "restore",
        checks.request("POST", f"/api/v1/documents/{document_id}/restore")[0],
        204,
    )
    checks.ok("  live again", checks.request("GET", "/api/v1/documents")[1]["total"], 2)
    # Restoring something already restored is a no-op, not an error: a client
    # retrying after a timeout should not be punished. Only a purge is terminal,
    # and that 409 needs an expired trash window to arrange (see src/api.rs).
    checks.ok(
        "restoring twice is still 204",
        checks.request("POST", f"/api/v1/images/{image_id}/restore")[0],
        204,
    )

    print("\n== a rendition can be uploaded again ==", flush=True)
    # The asymmetry this exists for: Atlas used to serve formats it would not
    # accept, so the file you had just downloaded could not be put back.
    for extension in ("jxl", "avif", "webp"):
        # Renditions are chosen by Accept, not by an extension in the path.
        status, rendition = checks.request(
            "GET",
            f"/media/{image_id}/w320",
            headers={"Accept": f"image/{extension}"},
            raw=True,
            token=None,
        )
        checks.ok(f"  fetched the {extension} rendition", status, 200)
        status, _ = checks.request(
            "POST",
            "/api/v1/images",
            rendition,
            {"Content-Type": f"image/{extension}"},
        )
        checks.ok(f"  and uploaded it back as image/{extension}", status, 201)

    print("\n== public delivery ==", flush=True)
    status, body = checks.request(
        "GET", f"/media/{image_id}/w320", raw=True, token=None
    )
    checks.ok("rendition serves", status, 200)
    checks.true(
        "  bytes look like an image",
        body[:4] in (b"RIFF", b"\xff\xd8\xff\xe0", b"\x89PNG", b"\0\0\0\x20ftyp"),
    )
    status, original = checks.request(
        "GET", f"/media/{image_id}/original", raw=True, token=None
    )
    checks.ok("original serves", status, 200)
    checks.ok("  is the uploaded bytes", original == png_bytes(), True)
    checks.ok(
        "wider than stored still answers",
        checks.request("GET", f"/media/{image_id}/w5000", raw=True, token=None)[0],
        200,
    )
    checks.ok(
        "document original serves",
        checks.request("GET", f"/media/{as_document['id']}/original", token=None)[0],
        200,
    )
    request = urllib.request.Request(
        checks.base_url + f"/media/{as_document['id']}/original"
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        disposition = response.headers.get("Content-Disposition", "")
    checks.true("document served as attachment", disposition.startswith("attachment"))
    checks.ok(
        "unknown id is a 404",
        checks.request("GET", "/media/nosuchid00000000000000/original", token=None)[0],
        404,
    )
    checks.ok(
        "traversal is refused",
        checks.request("GET", "/media/../../etc/passwd", token=None)[0],
        404,
    )


def check_example_client(checks: Checks, port: int) -> None:
    """Point the example consumer's real client at the real server.

    The album's 40 unit tests all run against a fake written by the same hand
    as the client, so they cannot see the client disagree with the server about
    the API. This is the check that caught it: Atlas spells a collection "image"
    in JSON but "images" in a URL, and the fake had been spelling both "images".
    """
    print("\n== example consumer's client ==", flush=True)
    album = REPO / "examples" / "family-album"
    sys.path.insert(0, str(album))
    try:
        from atlas_client import AtlasClient
    except ImportError as error:
        checks.skip(
            "album client",
            f"{error}.\n"
            "      Install the example's dependencies and run this script again:\n"
            "        python -m venv examples/family-album/.venv\n"
            "        examples/family-album/.venv/bin/pip install -q -r examples/family-album/requirements.txt\n"
            "        examples/family-album/.venv/bin/python scripts/smoke.py",
        )
        return

    client = AtlasClient(f"http://127.0.0.1:{port}", checks.token)
    import io

    image = client.upload(io.BytesIO(png_bytes()), "image/png", file_name="big.png")
    checks.ok("client upload", image.kind, "image")
    checks.ok("  collection for the route", image.collection, "images")
    checks.ok("  rendition count", len(image.renditions), 4)

    same = client.upload(io.BytesIO(png_bytes()), "image/png")
    checks.ok("client deduplicates", same.deduplicated, True)
    checks.ok("  to the same asset", same.id, image.id)

    stored = client.upload(io.BytesIO(png_bytes()), "image/png", collection="documents")
    checks.ok("a picture as a document", stored.kind, "document")
    checks.ok("  collection for the route", stored.collection, "documents")
    checks.ok("  with no renditions", len(stored.renditions), 0)

    listing = client.list_assets("images")
    checks.true(
        "client listing returns what it uploaded",
        any(entry["id"] == image.id for entry in listing["assets"]),
    )
    checks.ok("client srcset", len(client.srcset(image.id)["srcset"].split(", ")), 4)

    def fetched(collection: str, asset_id: str) -> Any:
        asset = client.get_asset(collection, asset_id)
        if asset is None:
            raise HarnessError(f"the client could not fetch {collection}/{asset_id}")
        return asset

    client.set_pinned("images", image.id, True)
    checks.ok("client pin", fetched("images", image.id).pinned, True)
    checks.ok(
        "wrong collection is not found", client.get_asset("documents", image.id), None
    )

    client.delete("images", image.id)
    checks.ok("client delete", fetched("images", image.id).state, "deleted")
    client.restore("images", image.id)
    checks.ok("client restore", fetched("images", image.id).state, "active")


def check_dashboard(checks: Checks, binary: Path, atlas_port: int, port: int) -> None:
    """Walk the CMS admin's real UI against this real Atlas process.

    This is not a mocked service test: upload, listing, detail, pin, delete,
    trash and restore all cross both actual HTTP servers. The dashboard binary
    is built separately because it is a standalone example crate.
    """
    print("\n== CMS admin consumer ==", flush=True)
    if not binary.is_file():
        checks.skip(
            "CMS admin",
            f"no binary at {binary}. Build it with: cargo build --manifest-path examples/cms-admin/Cargo.toml",
        )
        return

    with tempfile.TemporaryDirectory(prefix="atlas-dashboard-smoke-") as temporary:
        log_path = Path(temporary) / "dashboard.log"
        environment = dict(os.environ)
        environment.update(
            ATLAS_API_URL=f"http://127.0.0.1:{atlas_port}",
            ATLAS_API_TOKEN=TOKEN,
            ADMIN_PASSWORD="dashboard-smoke-password",
            ADMIN_LISTEN=f"127.0.0.1:{port}",
        )
        with log_path.open("wb") as log:
            process = subprocess.Popen(
                [str(binary)],
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                start_new_session=True,
            )
        base = f"http://127.0.0.1:{port}"
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise HarnessError(
                        f"CMS admin exited ({process.returncode}): "
                        f"{log_path.read_text(errors='replace')}"
                    )
                try:
                    with urllib.request.urlopen(f"{base}/login", timeout=1) as response:
                        if response.status == 200:
                            break
                except (urllib.error.URLError, OSError):
                    time.sleep(0.1)
            else:
                raise HarnessError(
                    f"CMS admin did not start within 15s: "
                    f"{log_path.read_text(errors='replace')}"
                )

            cookies = http.cookiejar.CookieJar()
            opener = urllib.request.build_opener(
                urllib.request.HTTPCookieProcessor(cookies)
            )

            def page(
                path: str, data: bytes | None = None, content_type: str | None = None
            ):
                headers = {"Content-Type": content_type} if content_type else {}
                request = urllib.request.Request(
                    base + path, data=data, headers=headers
                )
                try:
                    with opener.open(request, timeout=30) as response:
                        return response.status, response.read().decode(
                            "utf-8", "replace"
                        )
                except urllib.error.HTTPError as error:
                    return error.code, error.read().decode("utf-8", "replace")

            status, _login_page = page("/login")
            checks.ok("login page", status, 200)
            login_body = urllib.parse.urlencode(
                {"password": "dashboard-smoke-password", "next": "/library"}
            ).encode()
            status, _ = page("/login", login_body, "application/x-www-form-urlencoded")
            checks.ok("shared-password login", status, 200)
            status, library = page("/library")
            checks.ok("protected library", status, 200)
            checks.true(
                "library page renders", "Library" in library and "Images" in library
            )

            boundary = "atlas-dashboard-smoke-boundary"
            payload = png_bytes()
            multipart = (
                (
                    f'--{boundary}\r\nContent-Disposition: form-data; name="kind"\r\n\r\nimages\r\n'
                    f'--{boundary}\r\nContent-Disposition: form-data; name="files"; filename="dashboard.png"\r\n'
                    "Content-Type: image/png\r\n\r\n"
                ).encode()
                + payload
                + f"\r\n--{boundary}--\r\n".encode()
            )
            status, upload_result = page(
                "/upload", multipart, f"multipart/form-data; boundary={boundary}"
            )
            checks.ok("upload through dashboard", status, 200)
            checks.true("upload reports result", "dashboard.png" in upload_result)

            status, library = page("/library")
            checks.ok("uploaded asset appears in library", status, 200)
            checks.true(
                "grid previews point at Atlas's public media base",
                f"http://127.0.0.1:{atlas_port}/media/" in html.unescape(library),
            )
            asset = re.search(r'href="/assets/images/([a-z0-9]+)"', library)
            checks.true("library exposes a detail link", asset is not None)
            if asset is None:
                return
            asset_id = asset.group(1)

            status, detail = page(f"/assets/images/{asset_id}")
            checks.ok("asset detail", status, 200)
            checks.true(
                "detail offers a ready-made srcset", "Responsive image" in detail
            )
            checks.true(
                "detail links point at Atlas's public media base",
                f"http://127.0.0.1:{atlas_port}/media/" in html.unescape(detail),
            )
            checks.true(
                "dashboard does not surface consumer data yet",
                "Consumer data" not in detail,
            )

            status, _ = page(
                f"/assets/images/{asset_id}/pin",
                b"",
                "application/x-www-form-urlencoded",
            )
            checks.ok("pin action", status, 200)
            status, _ = page(
                f"/assets/images/{asset_id}/delete",
                b"",
                "application/x-www-form-urlencoded",
            )
            checks.ok("move to trash", status, 200)
            status, trash = page("/trash")
            checks.ok("global trash page", status, 200)
            checks.true(
                "trash entry has its collection",
                f"/trash/image/{asset_id}/restore" in trash,
            )
            status, _ = page(
                f"/trash/image/{asset_id}/restore",
                b"",
                "application/x-www-form-urlencoded",
            )
            checks.ok("restore using trash kind", status, 200)
            status, restored = page("/library")
            checks.ok("restored asset appears again", status, 200)
            checks.true("restored asset is listed", asset_id in restored)

            status, bulk = page(
                "/bulk",
                urllib.parse.urlencode(
                    {"kind": "images", "action": "pin", "ids": asset_id}
                ).encode(),
                "application/x-www-form-urlencoded",
            )
            checks.ok("bulk action through dashboard", status, 200)
            checks.true("  outcome is surfaced", "pin" in bulk.lower())

            # The same picture stored as a document must use documents for its
            # typed detail/delete routes, even though its MIME type is image/png.
            form = (
                (
                    f'--{boundary}\r\nContent-Disposition: form-data; name="kind"\r\n\r\ndocuments\r\n'
                    f'--{boundary}\r\nContent-Disposition: form-data; name="files"; filename="whole.png"\r\n'
                    "Content-Type: image/png\r\n\r\n"
                ).encode()
                + payload
                + f"\r\n--{boundary}--\r\n".encode()
            )
            status, document_upload = page(
                "/upload", form, f"multipart/form-data; boundary={boundary}"
            )
            checks.ok("image stored whole as a document", status, 200)
            checks.true("  upload was accepted", "whole.png" in document_upload)
            status, documents = page("/library?kind=documents")
            checks.ok("documents collection page", status, 200)
            document = re.search(r'href="/assets/documents/([a-z0-9]+)"', documents)
            checks.true("  document detail uses its collection", document is not None)
            if document is not None:
                document_id = document.group(1)
                status, _ = page(
                    f"/assets/documents/{document_id}/delete",
                    b"",
                    "application/x-www-form-urlencoded",
                )
                checks.ok("  document delete route", status, 200)
                status, trash = page("/trash")
                checks.ok("  document appears in trash", status, 200)
                checks.true(
                    "    with its collection",
                    f"/trash/document/{document_id}/restore" in trash,
                )
                status, _ = page(
                    f"/trash/document/{document_id}/restore",
                    b"",
                    "application/x-www-form-urlencoded",
                )
                checks.ok("    restores through documents", status, 200)

        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--bin", type=Path, default=REPO / "target" / "debug" / "atlas")
    parser.add_argument(
        "--dashboard-bin",
        type=Path,
        default=REPO / "examples" / "cms-admin" / "target" / "debug" / "cms-admin",
    )
    parser.add_argument(
        "--data",
        type=Path,
        default=Path(os.environ.get("TMPDIR", "/tmp")) / "atlas-smoke-data",
    )
    parser.add_argument(
        "--no-client", action="store_true", help="skip the example consumer's client"
    )
    parser.add_argument(
        "--keep-data",
        action="store_true",
        help="leave the data directory for inspection",
    )
    arguments = parser.parse_args()

    # A SIGTERM should still run the cleanup that stops the server.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))

    if arguments.data.exists() and not arguments.keep_data:
        import shutil

        shutil.rmtree(arguments.data)

    log = arguments.data.parent / "atlas-smoke.log"
    server = Server(arguments.bin, arguments.port, arguments.data, TOKEN, log)
    checks = Checks(server.base_url, TOKEN)

    with server:
        print(f"atlas pid {server.pid} on {server.base_url}", flush=True)
        check_api(checks)
        if not arguments.no_client:
            check_example_client(checks, arguments.port)
        check_dashboard(
            checks, arguments.dashboard_bin, arguments.port, arguments.port + 1
        )

    summary = f"{checks.total - checks.failures}/{checks.total} checks passed"
    if checks.skipped:
        summary += f", {checks.skipped} skipped"
    print(f"\n{summary}")
    return 1 if checks.failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except HarnessError as failure:
        print(f"\nharness error: {failure}", file=sys.stderr)
        sys.exit(2)
    except KeyboardInterrupt:
        print("\ninterrupted", file=sys.stderr)
        sys.exit(130)

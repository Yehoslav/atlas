"""The album's own database: folders, photos, and the posts that use them.

Every function here touches the album's content and nothing else. Media bytes
live in Atlas; this module only ever stores Atlas asset IDs.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from atlas_client import Rendition

SCHEMA_PATH = Path(__file__).with_name("schema.sql")


def utcnow() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


@dataclass(frozen=True)
class Photo:
    """A photo as the album knows it: an Atlas asset plus the album's meaning."""

    id: int
    asset_id: str
    # Which Atlas collection the asset is in, spelled as the route spells it:
    # "images" or "documents". Not meaning, just the address the album needs for
    # every later call about this asset.
    collection: str
    description: str
    alt_text: str
    tags: tuple[str, ...]
    folder_id: int | None
    folder_name: str | None
    content_type: str
    bytes: int
    width: int | None
    height: int | None
    uploaded_at: str
    renditions: tuple[Rendition, ...]

    @property
    def is_image(self) -> bool:
        return self.content_type.startswith("image/")


def connect(database: str | Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA foreign_keys = ON")
    return connection


def inserted_id(cursor: sqlite3.Cursor) -> int:
    """The id of the row just inserted.

    An `INSERT` always produces one, but the type says otherwise, and silently
    turning `None` into `0` would point the caller at some other row.
    """
    if cursor.lastrowid is None:  # pragma: no cover - unreachable for INSERT
        raise RuntimeError("the insert produced no row id")
    return cursor.lastrowid


def init(connection: sqlite3.Connection) -> None:
    connection.executescript(SCHEMA_PATH.read_text(encoding="utf-8"))
    migrate(connection)
    connection.commit()


def migrate(connection: sqlite3.Connection) -> None:
    """Bring an older album database up to the current schema.

    A real application would keep these as numbered migrations; this example
    has a couple, and the shape of the change is what matters: add a column
    with a default, backfill it, and keep working.
    """
    columns = {row["name"] for row in connection.execute("PRAGMA table_info(photos)")}
    if "kind" in columns and "collection" not in columns:
        # An earlier development build recorded Atlas's word for it ("image").
        # The route segment is what the album actually calls.
        connection.execute("ALTER TABLE photos RENAME COLUMN kind TO collection")
        connection.execute(
            "UPDATE photos SET collection ="
            " CASE WHEN collection = 'image' THEN 'images'"
            " WHEN collection = 'document' THEN 'documents' ELSE collection END"
        )
    if "collection" not in columns:
        connection.execute(
            "ALTER TABLE photos ADD COLUMN collection TEXT NOT NULL DEFAULT 'images'"
        )
        connection.execute(
            "UPDATE photos SET collection ="
            " CASE WHEN content_type LIKE 'image/%' THEN 'images' ELSE 'documents' END"
        )


# --- folders ---------------------------------------------------------------


def list_folders(connection: sqlite3.Connection) -> list[sqlite3.Row]:
    return connection.execute(
        "SELECT folders.*,"
        " (SELECT COUNT(*) FROM photos WHERE folder_id = folders.id) AS photo_count"
        " FROM folders ORDER BY position, name"
    ).fetchall()


def get_folder(connection: sqlite3.Connection, folder_id: int) -> sqlite3.Row | None:
    return connection.execute(
        "SELECT * FROM folders WHERE id = ?", (folder_id,)
    ).fetchone()


def create_folder(connection: sqlite3.Connection, name: str) -> int:
    position = connection.execute(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM folders"
    ).fetchone()[0]
    cursor = connection.execute(
        "INSERT INTO folders (name, position) VALUES (?, ?)", (name.strip(), position)
    )
    connection.commit()
    return inserted_id(cursor)


def delete_folder(connection: sqlite3.Connection, folder_id: int) -> None:
    connection.execute("DELETE FROM folders WHERE id = ?", (folder_id,))
    connection.commit()


# --- photos ----------------------------------------------------------------


def list_photos(
    connection: sqlite3.Connection,
    folder_id: int | None = None,
    tag: str | None = None,
) -> list[Photo]:
    """Photos, newest first. `tag` matches the album's own free-text tags."""
    sql = [
        "SELECT photos.*, folders.name AS folder_name"
        + " FROM photos LEFT JOIN folders ON folders.id = photos.folder_id"
    ]
    clauses: list[str] = []
    parameters: list[Any] = []
    if folder_id is not None:
        clauses.append("photos.folder_id = ?")
        parameters.append(folder_id)
    if tag:
        # Tags are the album's own comma-separated list. Atlas has no opinion
        # about them and is never asked about them.
        clauses.append("(',' || photos.tags || ',') LIKE ?")
        parameters.append(f"%,{tag.strip().lower()}%,")
    if clauses:
        sql.append("WHERE " + " AND ".join(clauses))
    sql.append("ORDER BY photos.uploaded_at DESC, photos.id DESC")

    rows = connection.execute(" ".join(sql), parameters)
    return [_to_photo(connection, row) for row in rows]


def get_photo(connection: sqlite3.Connection, photo_id: int) -> Photo | None:
    row = connection.execute(
        "SELECT photos.*, folders.name AS folder_name"
        " FROM photos LEFT JOIN folders ON folders.id = photos.folder_id"
        " WHERE photos.id = ?",
        (photo_id,),
    ).fetchone()
    return _to_photo(connection, row) if row else None


def get_photo_by_asset(connection: sqlite3.Connection, asset_id: str) -> Photo | None:
    row = connection.execute(
        "SELECT photos.*, NULL AS folder_name FROM photos WHERE asset_id = ?",
        (asset_id,),
    ).fetchone()
    return _to_photo(connection, row) if row else None


def add_photo(connection: sqlite3.Connection, asset: Any) -> int:
    """Record a freshly uploaded asset. The argument is Atlas's upload response."""
    cursor = connection.execute(
        "INSERT INTO photos (asset_id, collection, description, alt_text, tags, folder_id,"
        " content_type, bytes, width, height, uploaded_at)"
        " VALUES (?, ?, '', '', '', NULL, ?, ?, ?, ?, ?)",
        (
            asset.id,
            asset.collection,
            asset.content_type,
            asset.bytes,
            asset.width,
            asset.height,
            utcnow(),
        ),
    )
    photo_id = inserted_id(cursor)
    save_renditions(connection, asset.id, asset.renditions)
    connection.commit()
    return photo_id


def save_renditions(
    connection: sqlite3.Connection, asset_id: str, renditions: Iterable[Rendition]
) -> None:
    """Store the widths Atlas reported, so the album can write a correct srcset."""
    connection.executemany(
        "INSERT OR REPLACE INTO renditions (asset_id, max_width, width, height, formats)"
        " VALUES (?, ?, ?, ?, ?)",
        [
            (
                asset_id,
                rendition.max_width,
                rendition.width,
                rendition.height,
                ",".join(rendition.formats),
            )
            for rendition in renditions
        ],
    )


def update_photo(
    connection: sqlite3.Connection,
    photo_id: int,
    *,
    description: str,
    alt_text: str,
    tags: str,
    folder_id: int | None,
) -> None:
    """Editing a photo's meaning is an album operation. Atlas is not called."""
    connection.execute(
        "UPDATE photos SET description = ?, alt_text = ?, tags = ?, folder_id = ?"
        " WHERE id = ?",
        (
            description.strip(),
            alt_text.strip(),
            normalize_tags(tags),
            folder_id,
            photo_id,
        ),
    )
    connection.commit()


def delete_photo(connection: sqlite3.Connection, photo_id: int) -> None:
    connection.execute("DELETE FROM photos WHERE id = ?", (photo_id,))
    connection.commit()


def normalize_tags(tags: str) -> str:
    """Lowercase, deduplicate, keep the author's order."""
    ordered: dict[str, None] = {}
    for tag in tags.split(","):
        cleaned = tag.strip().lower()
        if cleaned:
            ordered[cleaned] = None
    return ", ".join(ordered)


def _to_photo(connection: sqlite3.Connection, row: sqlite3.Row) -> Photo:
    renditions = tuple(
        Rendition(
            max_width=stored["max_width"],
            width=stored["width"],
            height=stored["height"],
            formats=tuple(filter(None, stored["formats"].split(","))),
        )
        for stored in connection.execute(
            "SELECT max_width, width, height, formats FROM renditions"
            " WHERE asset_id = ? ORDER BY max_width",
            (row["asset_id"],),
        )
    )
    return Photo(
        id=row["id"],
        asset_id=row["asset_id"],
        collection=row["collection"],
        description=row["description"],
        alt_text=row["alt_text"],
        tags=tuple(tag for tag in (row["tags"] or "").split(", ") if tag),
        folder_id=row["folder_id"],
        folder_name=row["folder_name"],
        content_type=row["content_type"],
        bytes=row["bytes"],
        width=row["width"],
        height=row["height"],
        uploaded_at=row["uploaded_at"],
        renditions=renditions,
    )


# --- posts and references --------------------------------------------------


def list_posts(connection: sqlite3.Connection) -> list[sqlite3.Row]:
    return connection.execute(
        "SELECT posts.*,"
        " (SELECT COUNT(*) FROM post_photos WHERE post_id = posts.id) AS photo_count"
        " FROM posts ORDER BY COALESCE(published_at, created_at) DESC, id DESC"
    ).fetchall()


def get_post(connection: sqlite3.Connection, post_id: int) -> sqlite3.Row | None:
    return connection.execute("SELECT * FROM posts WHERE id = ?", (post_id,)).fetchone()


def create_post(connection: sqlite3.Connection, title: str, body: str = "") -> int:
    cursor = connection.execute(
        "INSERT INTO posts (title, body, created_at) VALUES (?, ?, ?)",
        (title.strip(), body.strip(), utcnow()),
    )
    connection.commit()
    return inserted_id(cursor)


def set_post_published(
    connection: sqlite3.Connection, post_id: int, published: bool
) -> None:
    connection.execute(
        "UPDATE posts SET published_at = ? WHERE id = ?",
        (utcnow() if published else None, post_id),
    )
    connection.commit()


def post_photos(connection: sqlite3.Connection, post_id: int) -> list[Photo]:
    photos = connection.execute(
        "SELECT photos.*, NULL AS folder_name FROM post_photos"
        " JOIN photos ON photos.id = post_photos.photo_id"
        " WHERE post_photos.post_id = ?"
        " ORDER BY post_photos.position, photos.id",
        (post_id,),
    ).fetchall()
    return [_to_photo(connection, row) for row in photos]


def attach_photo(connection: sqlite3.Connection, post_id: int, photo_id: int) -> None:
    position = connection.execute(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM post_photos WHERE post_id = ?",
        (post_id,),
    ).fetchone()[0]
    connection.execute(
        "INSERT OR IGNORE INTO post_photos (post_id, photo_id, position) VALUES (?, ?, ?)",
        (post_id, photo_id, position),
    )
    connection.commit()


def detach_photo(connection: sqlite3.Connection, post_id: int, photo_id: int) -> None:
    connection.execute(
        "DELETE FROM post_photos WHERE post_id = ? AND photo_id = ?",
        (post_id, photo_id),
    )
    connection.commit()


def references_for_photo(
    connection: sqlite3.Connection, photo_id: int, published_only: bool = True
) -> list[sqlite3.Row]:
    """Posts that use a photo.

    This is the check Atlas cannot make on the album's behalf: only the album
    knows that "Grandma's 70th" is published and shows this photo on the front
    page.
    """
    sql = (
        "SELECT posts.* FROM posts JOIN post_photos ON post_photos.post_id = posts.id"
        " WHERE post_photos.photo_id = ?"
    )
    if published_only:
        sql += " AND posts.published_at IS NOT NULL"
    return connection.execute(sql + " ORDER BY posts.id", (photo_id,)).fetchall()


def photo_is_referenced(connection: sqlite3.Connection, photo_id: int) -> bool:
    """Whether any post at all - draft or published - uses this photo."""
    return bool(
        connection.execute(
            "SELECT EXISTS (SELECT 1 FROM post_photos WHERE photo_id = ?)", (photo_id,)
        ).fetchone()[0]
    )

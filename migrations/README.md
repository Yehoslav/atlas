# Atlas schema migrations

`sqlx::migrate!("./migrations")` embeds every `*.sql` file in this directory and
applies the ones the database has not seen, in version order, at startup. The
`_sqlx_migrations` table records each migration with a checksum, so editing an
applied migration is refused instead of silently ignored.

`build.rs` writes the embedded file list into `OUT_DIR` and `src/lib.rs`
includes it. That is deliberate: the macro re-runs when an existing file
changes (`include_str!` tracks it) *and* when a new file appears, which neither
`sqlx migrate build-script` nor `RUSTFLAGS='--cfg sqlx_macros_unstable'` would
guarantee on stable Rust.

## What `ALTER TABLE` can and cannot do in SQLite

SQLite supports exactly four direct `ALTER TABLE` operations:

| Operation | Supported | Notes |
| --- | --- | --- |
| `ALTER TABLE t RENAME TO u` | yes | Also rewrites references to `t` in other tables, triggers and views |
| `ALTER TABLE t RENAME COLUMN a TO b` | yes | Since 3.25 |
| `ALTER TABLE t ADD COLUMN ...` | yes | See the restrictions below |
| `ALTER TABLE t DROP COLUMN c` | yes | Since 3.35, and not for indexed, unique, or constrained columns |

Anything else — changing a column's type, adding or dropping a constraint,
adding a foreign key, changing `NOT NULL` or a default, or making a column part
of the primary key — is **not** supported. `ALTER TABLE ... ALTER COLUMN` is not
SQLite syntax at all; reaching for it means writing a PostgreSQL migration by
accident.

`ADD COLUMN` is further restricted: the column may not have a `PRIMARY KEY` or
`UNIQUE` constraint, may not default to an expression or `CURRENT_TIMESTAMP`,
and a `NOT NULL` column must supply a constant non-null default.

Atlas keeps `source_format`, `bytes`, `sha256`, `width`, `height`, and
`created_at` exactly as they are for that reason: narrowing or widening a type
means rebuilding the table, which is the one operation on this list that can
destroy data without any error being raised.

## Prefer additive migrations

Adding a column is instant, rewrites no rows, and fires no cascades. To change
something in practice:

1. Add the new column, nullable or with a constant default.
2. Backfill it in a second migration, in batches if the table is large.
3. Ship the code that reads and writes both shapes.
4. Drop the old column in a later release.

Never rename-and-rebuild in a single step. Atlas has a small database; a
four-step rollout still costs less than one that drops every `renditions` row
without saying so.

## Rebuilding a table

When a rebuild is genuinely unavoidable, use the procedure from the SQLite
documentation, with one Atlas-specific hazard:

- `PRAGMA foreign_keys = OFF` must be issued **outside** the transaction.
  Inside one it is a silent no-op.
- The pragma applies to a single connection. With a connection pool, acquire one
  connection and run the whole rebuild on it.
- `DROP TABLE assets` fires `ON DELETE CASCADE` on `renditions`. With
  enforcement left on, the rebuild completes, the parent table comes back, and
  the database looks perfectly self-consistent while every rendition is gone.
  `PRAGMA foreign_key_check` finds nothing to complain about, because the
  cascade deleted the orphan instead of leaving it.
- Run `PRAGMA foreign_key_check` after the rebuild and roll back if it reports
  anything.
- Recreate indexes, triggers, and views by hand; the rebuild drops them with the
  old table.

```sql
PRAGMA foreign_keys = OFF;          -- outside the transaction
BEGIN;
CREATE TABLE assets_new ( ... );
INSERT INTO assets_new ( ... ) SELECT ... FROM assets;
DROP TABLE assets;                  -- cascades here only if the pragma was ignored
ALTER TABLE assets_new RENAME TO assets;
CREATE INDEX assets_by_deleted_at ON assets (deleted_at);
COMMIT;
PRAGMA foreign_keys = ON;
PRAGMA foreign_key_check;
```

`guarded_table_rebuild_keeps_child_rows` in `src/store.rs` runs exactly this
sequence, and `unguarded_table_rebuild_cascades_child_rows_away` runs the same
rebuild without the guard to prove the cascade really does delete the child rows
silently. Run both before trusting a rebuild.

## Uniqueness Atlas does not enforce

`renditions` rows are unique per `(asset_id, max_width, format)`, and `assets.id`
is the primary key. Nothing else is unique, and that is deliberate.

Uploads deduplicate identical bytes by looking for a live asset with the same
`sha256` before converting. Making that a constraint would need

```sql
CREATE UNIQUE INDEX assets_live_sha256 ON assets (sha256) WHERE purged_at IS NULL;
```

which cannot be created on a database that already holds two live assets with
the same bytes — and making room for it means deleting one of them, along with
its files, at the moment someone runs a migration. A row a published post still
references does not care that its bytes are a duplicate. So the lookup stays
best effort: two *concurrent* uploads of identical bytes can both become assets,
and the window is one conversion wide.

## Adding a migration

1. Create `migrations/<next_version>_<description>.sql`. Never renumber or edit
   an applied migration.
2. Keep it additive where you can. `ALTER TABLE ... ADD COLUMN` and
   `CREATE INDEX` are safe on a live database.
3. `cargo test` — Atlas refuses to start on an integrity or foreign key
   violation, so a broken migration fails loudly on the next boot.

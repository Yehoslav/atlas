# Deploying Atlas

How to run Atlas on a real host: the systemd and container paths, the reverse
proxy, upgrades, and backing up the two halves that are useless separately.

## Deploying Atlas

Production targets ordinary Linux hosts, not specifically NixOS. Atlas is one
process next to the site it serves, behind that site's reverse proxy, with a
private directory of its own.

There are two ways to deploy it: a systemd unit on a host, or a container
image. Both need the same three things — the binary, an ImageMagick with the
codecs, and a private data directory — and both put the token in a file
rather than on a command line.

### What a host needs

- **The binary.** `cargo build --release` produces `target/release/atlas`. It
  links SQLite statically, so there is no SQLite library to install.
- **ImageMagick with the codecs.** Atlas reads and writes JPEG, PNG, WebP,
  AVIF, JPEG XL and HEIC, and converts colour profiles through LCMS. It
  refuses to start without them, which is the right refusal: a build without
  them fails on somebody's first upload, or on a phone photograph, long
  after the cause. Check a host before installing:

  ```sh
  bash scripts/check-magick-codecs.sh
  ```

  This is the check the development shell makes as it opens, and the one the
  container image makes as it builds. It names the codec that is missing
  instead of leaving it to be discovered later.

  Getting those codecs is the awkward part of a host install, and it is worth
  saying plainly: **Debian and Ubuntu do not ship an ImageMagick that can
  write JPEG XL.** Debian bookworm's is 6.9.11 from 2021 with no JPEG XL
  coder at all, and Debian trixie's 7.1.1 has AVIF and HEIC as read-only. A
  host install therefore means either building ImageMagick from source with
  `--with-jxl` and `--with-heic`, or taking the ImageMagick from Nixpkgs,
  which is the same build the development environment and the container image
  use. The container image is the easier of the two on a small host.
- **A private data directory**, mode `0700`, owned by the service account.
  Atlas refuses to start against a non-empty directory that group or other
  users can reach.

### The environment file

The token is a secret and lives in a file, not in a unit file and not on a
command line where it would show up in `ps`. `contrib/atlas.env.example` is a
starting point:

```sh
useradd --system --user-group --home-dir /var/lib/atlas \
    --no-create-home --shell /usr/sbin/nologin atlas
install -d -o atlas -g atlas -m 0750 /etc/atlas
install -o atlas -g atlas -m 0600 contrib/atlas.env.example /etc/atlas/atlas.env
$EDITOR /etc/atlas/atlas.env
```

```ini
ATLAS_API_TOKEN=the-long-random-string-openssl-rand-hex-32-gives
ATLAS_LISTEN_ADDR=127.0.0.1:3000
ATLAS_DATA_DIR=/var/lib/atlas
ATLAS_VARIANT_WIDTHS=320,640,960,1280
```

The token authorises every write and every metadata read, and it never
reaches a browser: the consuming application keeps it on its own server. The
widths belong in this file *before the first upload* — an asset keeps the
widths it was uploaded with, and
`POST /api/v1/images/{id}/renditions` adds the ones that are missing, so a
wrong setting is repairable but not free.

The other settings are described under [Configuration](configuration.md). A
TOML file is the better place for the non-secret shape of a deployment; the
environment file is the better place for the token.

### The systemd unit

`contrib/atlas.service` runs the service as its own account, in the
foreground, with the environment file and the data directory handled:

```sh
install -m 0755 target/release/atlas /usr/local/bin/atlas
install -m 0644 contrib/atlas.service /etc/systemd/system/atlas.service
systemctl daemon-reload
systemctl enable --now atlas
```

The unit's `StateDirectory=atlas` creates `/var/lib/atlas` as the service
account owns it, with mode `0700`, before the first start. Nothing else has to
create it and nothing else has to remember to make it private — which is the
detail that is easy to get wrong and that Atlas refuses to start without.

### The container image

The `Dockerfile` builds the service and an ImageMagick that has the codecs.
Build it where there are cores and memory — a CI runner, a laptop, anything
bigger than the server it is going to run on.

The repository's Actions workflow builds and smoke-tests the image on every
change. After the PR that updates `Cargo.toml` is merged, tag that merged
commit to publish it to GitHub Container Registry. Use the SemVer version
from `Cargo.toml` verbatim, including its pre-release label; the workflow
checks that the tag matches. For the current `0.1.0-alpha.3` crate version:

```sh
jj git fetch --remote origin
jj tag set 0.1.0-alpha.3 -r master@origin
jj git push --tag 0.1.0-alpha.3
```

The server pulls the versioned image rather than compiling the Rust tree:

```sh
docker pull ghcr.io/yehoslav/atlas:0.1.0-alpha.3
```

The floating `latest` image tag is updated only by stable SemVer tags; an
alpha release never becomes `latest`.

The first published GHCR package may be private by default. To let the server
pull without credentials, open the package's **Package settings** on GitHub and
change its visibility to public. Otherwise, configure a read-only GitHub
token with `read:packages` and log in to `ghcr.io` on the server.

Pull a *version*, never a floating tag: a floating tag changes the bytes under
a running service, which is the same mistake as a mutable URL. A server with
no registry to talk to can carry the image as a file instead:

```sh
docker save ghcr.io/yehoslav/atlas:0.1.0-alpha.3 | gzip > atlas-0.1.0-alpha.3.tar.gz
# on the server
gunzip -c atlas-0.1.0-alpha.3.tar.gz | docker load
```

Copy `contrib/atlas.container.env.example` to `/etc/atlas/atlas.container.env`,
set the token, and make the file readable only by root. Run it with that file,
the data directory on a volume, and the port published to the host's loopback:

```sh
sudo install -o root -g root -m 0600 contrib/atlas.container.env.example /etc/atlas/atlas.container.env
sudoedit /etc/atlas/atlas.container.env

sudo docker run --detach --name atlas \
    --publish 127.0.0.1:3000:3000 \
    --env-file /etc/atlas/atlas.container.env \
    --volume atlas-data:/var/lib/atlas \
    ghcr.io/yehoslav/atlas:0.1.0-alpha.3
```

Two details worth knowing. A **named volume** inherits its ownership from the
image, so the service account can write to it; a **bind mount** takes the host
directory's ownership instead, and the service account is uid 100 and gid 101,
so the host directory has to be given to it (`chown 100:101`) or every upload
fails with a permission error. And the port is published to the host's
loopback on purpose: the media is meant to be public, but through the site's
own proxy, not through a port the whole internet can reach.

### The reverse proxy

Atlas expects the site's HTTPS proxy in front of it. The proxy routes
`/media/` publicly; the consuming application's backend calls the API on the
host's loopback directly, so the API and token never reach the browser:

```nginx
location /media/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
}

```

Proxy path matching must be case-preserving: asset IDs are lowercase, and a
proxy that folds case turns a valid ID into a `404`.

### The health check

`GET /healthz` answers `{"status":"ok"}` once the service is up. It is worth
more than a liveness ping, because the service only starts serving after it
has applied the schema migrations and run `PRAGMA integrity_check` and
`PRAGMA foreign_key_check` — a database that fails either of those stops the
service from starting at all, with the reason in the log. A `200` from
`/healthz` therefore means the database is sound, not merely that a process
answers.

### Upgrading

The schema migrations run at startup, so stop the old process before replacing
the binary or image.

For a container deployment, bump the version in `Cargo.toml`, push that exact
SemVer tag (for example, `0.1.0-alpha.3`), then pull and
replace the container. Removing the container does not remove its named data
volume:

```sh
sudo docker pull ghcr.io/yehoslav/atlas:X.Y.Z-alpha.N
sudo docker stop atlas
sudo docker rm atlas
sudo docker run --detach --name atlas \
    --publish 127.0.0.1:3000:3000 \
    --env-file /etc/atlas/atlas.container.env \
    --volume atlas-data:/var/lib/atlas \
    ghcr.io/yehoslav/atlas:X.Y.Z-alpha.N
sudo docker logs atlas
```

For a systemd deployment:

```sh
systemctl stop atlas
install -m 0755 target/release/atlas /usr/local/bin/atlas
systemctl start atlas
journalctl -u atlas
```

For either deployment, finish with `curl -s localhost:3000/healthz`, which
should answer `{"status":"ok"}`.

The migrations are additive and guarded, so the failure mode is that the
service does not start and the log says why. A failed migration rolls back
that migration; earlier successfully applied migrations remain recorded. The
service refuses to serve rather than serving from a half-migrated database.
Fix the cause in the log and start the new version again. Nothing else is
needed around a successful upgrade: the files are unchanged, the URLs that were
handed out keep working, and the assets are the same assets.

The upgrade that introduces [generations](api.md#generations) changes what a
consumer is given, though not what it already has. Every asset reads as
generation 1 and its existing URLs keep their bytes, but the rendition URLs in
responses now carry `?g=1`, and the bare `/media/{id}/w{width}` is cached for
an hour instead of a year. A consumer should re-read its srcsets after the
upgrade and store the `?g=` URLs. Until it does, there is one trap: a browser
or CDN that cached a bare URL before the upgrade holds it under the old
year-long `immutable` policy, so after a re-encode it keeps serving the bad
encoding until that copy expires — the very thing the re-encode exists to fix.
Switch the consumer to the URLs the re-encode answers with, or purge those
bare URLs from the CDN.

## Backup and restore

Metadata and files are useless separately: a restore that pairs a database with
a different set of `assets/` directories makes every asset undeliverable. Back
both up, and drill the restore.

**Snapshot the database with `VACUUM INTO`, never `cp`.** Atlas runs SQLite in
WAL mode, where committed transactions may still live in `atlas.sqlite3-wal`.
Copying `atlas.sqlite3` on its own captures neither the WAL nor a consistent
cut, and the result may be a database that opens fine and is missing rows.
`VACUUM INTO` takes a read lock, writes a single consistent file, and compacts
it as a side effect:

```sh
sqlite3 "$ATLAS_DATA_DIR/atlas.sqlite3" \
  "VACUUM INTO '/backup/atlas-$(date -u +%Y%m%dT%H%M%SZ).sqlite3'"
```

**SQLite does not `fsync` the `VACUUM INTO` output.** A crash or a full disk
during the write can leave a short file that still opens. A backup job that
assumes success is a backup that does not exist, so verify each snapshot:

```sh
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 'PRAGMA integrity_check;'
sqlite3 /backup/atlas-20260101T000000Z.sqlite3 \
  'SELECT COUNT(*) FROM assets; SELECT COUNT(*) FROM renditions;
   SELECT COUNT(*) FROM superseded_renditions;'
```

`PRAGMA quick_check` is enough to catch a truncated file; `integrity_check` is
the stricter version. Keep the output, and compare the counts with the live
database (`SELECT COUNT(*) FROM assets`) at a quiet moment.

**Then copy the files.** A file copy only has to be internally consistent, so
`snapshot`, `rsync -a`, or `tar` are all fine:

```sh
rsync -a --delete "$ATLAS_DATA_DIR/assets/" /backup/assets/
rsync -a --delete "$ATLAS_DATA_DIR/trash/"  /backup/trash/
```

Snapshot the database *first*: the file copy taken afterwards may contain an
asset the database does not know about (harmless — startup recovery removes
unindexed directories) rather than missing one (broken delivery until the next
upload). Note that files copied while Atlas is running can be mid-upload; the
staging directory is never published, so ignore it and let startup recovery
clear it.

**Restore drill.** Stop Atlas, restore both halves, start it, and check:

1. `GET /healthz` returns `ok` — this already proves the migrations applied and
   both `PRAGMA` checks passed.
2. `GET /api/v1/images/{id}` for a known asset returns its metadata.
3. `GET /media/{id}/original` returns `200` with the expected length, not a
   placeholder.

A restore drill that has not fetched an actual file has not tested the thing
that matters.

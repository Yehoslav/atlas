# Security

## Reporting a vulnerability

Please report security problems privately, through GitHub's private
vulnerability reporting on this repository
(**Security → Report a vulnerability**). That opens a private thread between
you and the maintainer.

Please do not open a public issue for a vulnerability, and please do not
disclose one in a pull request. Give the maintainer a chance to fix it and
release a version before it becomes public.

A useful report says what an attacker can do, how to do it, and which version
you tested. A proof of concept is much better than a description. You should
get an acknowledgement within a few days, and an assessment of whether it is a
vulnerability shortly after.

## What is in scope

- **Media path handling.** Public media URLs are resolved to files on disk.
  Anything that lets a request reach a file it should not — traversal, a
  surprising encoding, a symlink — is a vulnerability.
- **The upload boundary.** Atlas treats every upload as untrusted, including
  an administrator's. The byte limits, the dimension and pixel caps, the
  signature checks and the ImageMagick argument handling are the boundary.
  Anything that gets past them is worth knowing about.
- **Credential handling.** The API token authorises every write. Anything that
  reveals it, accepts it from somewhere it should not, or lets it reach a
  browser is a vulnerability.
- **Deletion and retention.** Bytes that should be gone, or should still be
  there and are not, are worth a report: the trash lifecycle and the pin
  mechanism are what stand between a mistaken delete and a broken page.

## What is expected, and not a vulnerability

- **The API is not meant to be public.** It is authenticated by a bearer
  token and is meant to sit behind the site's own reverse proxy, reachable
  only from the server or a CLI. A deployment that exposes it to the internet
  is a deployment that has made the token the only thing standing between an
  attacker and every media file.
- **Atlas cannot know what its consumer does with media.** It stores the
  bytes, the metadata and one boolean — the pin — that a consumer sets when it
  still uses an asset. It has no idea which pages use which pictures; that
  belongs to the consuming site's own database.
- **The consuming applications are separate projects.** The examples in this
  repository are examples, and their own bugs are their own. Do report
  anything that reaches Atlas through them, though.
- **A prototype's rough edges.** The HTTP contract is pre-1.0 and may change.
  That is not a vulnerability, but it may be a bug worth reporting.

## Supported versions

The current release on the default branch is the supported one. There is no
long-term-support branch: Atlas is deployed as one instance per site, and
upgrading is a matter of replacing a binary and letting the migrations run.

## Deployment notes worth repeating

- Run Atlas on loopback, behind the site's HTTPS reverse proxy, and route
  `/media/` publicly while keeping the API server-side.
- Keep the data directory private to the service account. Atlas refuses to
  start if an existing, non-empty data directory is readable by other users.
- Keep the API token out of browser code and out of version control. It is a
  bearer token: whoever holds it can upload, delete and change every asset.
- Back up the database and the media directory together, and drill the
  restore. The README's backup section has the details; a backup that has
  never been restored is not a backup.

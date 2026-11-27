#!/usr/bin/env bash
# Check that the three places naming the Rust version agree.
#
# Each consumer needs its own syntax: Cargo reads `rust-version`, the image
# builder reads the `rust:` tag, and rust-overlay wants a release number. A
# comment in each file says to keep them together; this is what makes drift
# fail instead of being noticed later, on a build that used a compiler the
# crate does not claim.
#
# The patch release is not compared: `1.90`, `1.90-alpine` and `1.90.0` are the
# same MSRV, and the image tag follows the latest 1.90.x while the flake pins
# one release.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

version_in() {
    sed -n "$2" "$root/$1" | head -n 1
}

cargo="$(version_in Cargo.toml 's/^rust-version = "\([0-9][0-9.]*\)"$/\1/p')"
docker="$(version_in Dockerfile 's/^FROM rust:\([0-9][0-9.]*\)-alpine AS build$/\1/p')"
flake="$(version_in flake.nix 's/.*rust-bin\.stable\."\([0-9][0-9.]*\)"\.default.*/\1/p')"

for pair in "Cargo.toml:$cargo" "Dockerfile:$docker" "flake.nix:$flake"; do
    if [ -z "${pair#*:}" ]; then
        printf 'cannot find the Rust version in %s\n' "${pair%%:*}" >&2
        exit 1
    fi
done

# `1.90` and `1.90.0` are the same floor; compare major.minor only.
major_minor() {
    local major rest
    major="${1%%.*}"
    rest="${1#*.}"
    printf '%s.%s' "$major" "${rest%%.*}"
}

expected="$(major_minor "$cargo")"
status=0
for pair in "Cargo.toml:$cargo" "Dockerfile:$docker" "flake.nix:$flake"; do
    file="${pair%%:*}"
    value="${pair#*:}"
    if [ "$(major_minor "$value")" != "$expected" ]; then
        printf '%s pins %s, Cargo.toml pins %s\n' "$file" "$value" "$cargo" >&2
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    printf 'The Rust version must agree in Cargo.toml, Dockerfile and flake.nix.\n' >&2
    exit 1
fi

printf 'Rust %s is pinned consistently in Cargo.toml, Dockerfile and flake.nix.\n' "$expected"

#!/usr/bin/env bash
# Prove that scripts/check-rust-version.sh notices a partial bump.
#
# A test that only ran the check against the repository would prove it passes
# today, not that it fails when one of the three files is left behind, which is
# the only thing it exists to catch.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
checker="$here/check-rust-version.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/scripts"
cp "$checker" "$work/scripts/check-rust-version.sh"

write_pins() {
    printf 'rust-version = "%s"\n' "$1" > "$work/Cargo.toml"
    printf 'FROM rust:%s-alpine AS build\n' "$2" > "$work/Dockerfile"
    printf 'rust = pkgs.rust-bin.stable."%s".default;\n' "$3" > "$work/flake.nix"
}

run() { bash "$work/scripts/check-rust-version.sh" >/dev/null 2>&1; }

# Agreeing pins pass, including the patch form the flake uses.
write_pins 1.90 1.90 1.90.0
run || {
    echo "matching pins were rejected" >&2
    exit 1
}

# Any one of the three left behind fails.
for file in Cargo.toml Dockerfile flake.nix; do
    case "$file" in
        Cargo.toml) write_pins 1.91 1.90 1.90.0 ;;
        Dockerfile) write_pins 1.90 1.91 1.90.0 ;;
        flake.nix) write_pins 1.90 1.90 1.91.0 ;;
    esac
    if run; then
        echo "$file was not checked" >&2
        exit 1
    fi
done

# A missing pin is a failure, not a silent pass.
rm "$work/flake.nix"
if run; then
    echo "a missing flake pin was accepted" >&2
    exit 1
fi

echo "the Rust version check fails on drift"

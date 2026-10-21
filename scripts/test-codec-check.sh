#!/usr/bin/env bash
# Prove that scripts/check-magick-codecs.sh reads ImageMagick's codec
# listing in both of the shapes it is printed in.
#
# A build without loadable modules prints the name, the mode and the
# description:
#
#     JPEG* rw-   Joint Photographic Experts Group JFIF format
#
# A build with them prints the module the format lives in between the name
# and the mode, which is how Debian and Ubuntu ship ImageMagick:
#
#     JPEG  JPEG      rw-   Joint Photographic Experts Group JFIF format
#
# The mode used to be read as the second column, which is the module on such
# a build and the mode on the other. The check then found nothing that reads
# and writes and named every codec missing, on the one host that had them all.
#
# The check is asked the same question of both shapes through a fake `magick`
# on PATH, so neither a second ImageMagick build nor a Debian host is needed
# to keep the answer honest.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
checker="$here/check-magick-codecs.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The codecs Atlas needs, in the shape a build without modules prints them.
static_listing='   Format  Mode  Description
-------------------------------------------------------------------------------
     JPEG* rw-   Joint Photographic Experts Group JFIF format (libjpeg-turbo)
      PNG* rw-   Portable Network Graphics (libpng 1.6.58)
     WEBP* rw+   WebP Image Format (libwebp 1.6.0)
     AVIF  rw+   AV1 Image File Format (1.23.1)
      JXL* rw+   JPEG XL (ISO/IEC 18181) (libjxl 0.12.0)
     HEIC  rw+   High Efficiency Image Format (1.23.1)'

# The same codecs in the shape a build with modules prints them.
module_listing=' Format  Module    Mode  Description
-------------------------------------------------------------------------------
    JPEG  JPEG      rw-   Joint Photographic Experts Group JFIF format (libjpeg-turbo)
     PNG  PNG       rw-   Portable Network Graphics (libpng 1.6.58)
    WEBP  WEBP      rw+   WebP Image Format (libwebp 1.6.0)
    AVIF  HEIC      rw+   AV1 Image File Format (1.23.1)
     JXL  JXL       rw+   JPEG XL (ISO/IEC 18181) (libjxl 0.12.0)
    HEIC  HEIC      rw+   High Efficiency Image Format (1.23.1)'

# A build that has every codec but cannot write HEIC.
without_heic=' Format  Module    Mode  Description
-------------------------------------------------------------------------------
    JPEG  JPEG      rw-   Joint Photographic Experts Group JFIF format (libjpeg-turbo)
     PNG  PNG       rw-   Portable Network Graphics (libpng 1.6.58)
    WEBP  WEBP      rw+   WebP Image Format (libwebp 1.6.0)
    AVIF  HEIC      rw+   AV1 Image File Format (1.23.1)
     JXL  JXL       rw+   JPEG XL (ISO/IEC 18181) (libjxl 0.12.0)
    HEIC  HEIC      r--   High Efficiency Image Format (1.23.1)'

cat >"$work/magick" <<'EOF'
#!/usr/bin/env bash
cat "$FAKE_MAGICK_LISTING"
EOF
chmod +x "$work/magick"

failures=0

# Run the check against a listing and print what it said.
run_check() {
    local listing="$1"
    printf '%s\n' "$listing" >"$work/listing"
    FAKE_MAGICK_LISTING="$work/listing" PATH="$work:$PATH" \
        bash "$checker" 2>&1 || true
}

expect_present() {
    if ! run_check "$1" | grep -q "has every codec"; then
        echo "FAIL: the check did not accept a listing with every codec" >&2
        failures=$((failures + 1))
    fi
}

expect_missing() {
    local listing="$1"
    local codec="$2"
    local output
    output="$(run_check "$listing")"
    if ! grep -q "missing codecs for:.*$codec" <<<"$output"; then
        echo "FAIL: the check did not name $codec as missing: $output" >&2
        failures=$((failures + 1))
    fi
}

expect_present "$static_listing"
expect_present "$module_listing"
expect_missing "$without_heic" "HEIC"

if [ "$failures" -ne 0 ]; then
    echo "$failures codec check test(s) failed." >&2
    exit 1
fi

echo "The codec check reads both listing shapes."

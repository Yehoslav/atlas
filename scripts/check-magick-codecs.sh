#!/usr/bin/env bash
# Check that the ImageMagick on PATH can read and write every codec Atlas
# needs.
#
# Atlas makes the same check at startup (`ImageProcessor::verify_formats`) and
# refuses to run without them. This says so earlier, where the reason can
# still be read, and names the codec that is missing instead of leaving a
# nixpkgs bump to be discovered as a phone photograph that will not upload.
#
# The list is Atlas's own: a format that is written out must also be readable
# back, and uploads may arrive in AVIF, JPEG XL or HEIC.
#
# This file is also embedded in the dev shell's `shellHook`, so it sets no
# shell options: `set -u` in an interactive shell makes unbound variables an
# error, and that is not this file's business.

codecs="JPEG PNG WEBP AVIF JXL HEIC"

formats="$(magick -list format 2>/dev/null || true)"
missing=""
for codec in $codecs; do
    # A format is listed as its name, a `*` when ImageMagick handles it
    # natively, and a mode of three letters: read, write, and whether it
    # holds more than one image.
    #
    # The mode is found by what it looks like rather than by where it sits.
    # A build without loadable modules prints the name, the mode and the
    # description; a build with them prints the module the format lives in
    # between the name and the mode, which is how Debian and Ubuntu ship
    # ImageMagick. The mode is lower case and the module names are upper
    # case, so a module name can never be mistaken for a mode.
    if ! awk -v want="$codec" '
        $1 == want || $1 == want "*" {
            mode = ""
            for (i = 2; i <= NF; i++) {
                if ($i ~ /^[rw-][rw-][+-]$/) { mode = $i; break }
            }
            if (mode ~ /r/ && mode ~ /w/) { found = 1 }
        }
        END { exit found ? 0 : 1 }
    ' <<<"$formats"; then
        missing="$missing $codec"
    fi
done

if [ -n "$missing" ]; then
    echo "ImageMagick is missing codecs for:$missing" >&2
    echo "Atlas needs them to read and write what it accepts, and refuses to start without them." >&2
    echo "'magick -list format' shows what this build has." >&2
    exit 1
fi

echo "ImageMagick has every codec Atlas needs."

# Atlas in a container.
#
# The service is a static musl binary and the ImageMagick it shells out to is
# Alpine's, with the codecs Atlas needs: JPEG, PNG, WebP, AVIF, JPEG XL and
# HEIC, and LCMS to convert a photo's colour profile to sRGB.
#
# The build runs the repository's own codec check, so an image that is missing
# one of them cannot be produced: the build fails and names the codec, rather
# than the image starting and refusing to serve.
#
#     docker build -t atlas .
#     docker run --rm -p 127.0.0.1:3000:3000 \
#         --env-file /etc/atlas/atlas.container.env \
#         -v atlas-data:/var/lib/atlas \
#         atlas
#
# The volume holds the database and the media files, and nothing else does.
# The token is read from a private environment file, not from the command
# line, where it would be visible in `ps`.

# --- the service ------------------------------------------------------------

FROM rust:1.90-alpine AS build

# `musl-dev` is the C library the musl target links against, and `pkgconfig`
# is what the build scripts ask for. SQLite is compiled into the binary, so
# there is no SQLite library to install here or in the image.
RUN apk add --no-cache musl-dev pkgconfig

WORKDIR /src
COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl

# --- the runtime ------------------------------------------------------------

FROM alpine:3.21

# The JPEG XL and WebP coders and the HEIC delegate are separate packages
# here, and LCMS comes with ImageMagick itself.
RUN apk add --no-cache imagemagick imagemagick-jxl imagemagick-webp libheif

# The codec check the development shell and CI run, run here as well. It
# needs bash for a here-string, which the image does not otherwise want, so
# bash is removed again afterwards.
COPY scripts/check-magick-codecs.sh /tmp/check-magick-codecs.sh
RUN apk add --no-cache bash \
 && bash /tmp/check-magick-codecs.sh \
 && apk del bash \
 && rm /tmp/check-magick-codecs.sh

COPY --from=build /src/target/x86_64-unknown-linux-musl/release/atlas /usr/local/bin/atlas

# The service account and its data directory. Atlas refuses to start against
# a data directory that other users can reach, so the directory is created
# here with the right owner and mode rather than being left to the first
# start. A named volume inherits them from the image.
#
# The group is made first and named: busybox's `adduser -S` would otherwise
# put the account in `nogroup`, and there would be no group to give the
# directory to.
RUN addgroup -S atlas \
 && adduser -S -H -h /var/lib/atlas -s /sbin/nologin -G atlas atlas \
 && mkdir -p /var/lib/atlas \
 && chown atlas:atlas /var/lib/atlas \
 && chmod 700 /var/lib/atlas

ENV ATLAS_DATA_DIR=/var/lib/atlas
# Inside a container the service is behind the host's proxy, and a listener
# on the container's own loopback would be unreachable from the host. Publish
# the port to the host's loopback only: `-p 127.0.0.1:3000:3000`.
ENV ATLAS_LISTEN_ADDR=0.0.0.0:3000

VOLUME ["/var/lib/atlas"]
USER atlas
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/atlas"]

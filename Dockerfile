# syntax=docker/dockerfile:1.7

# The build stage runs on the builder's own architecture and cross-compiles,
# so an arm64 image does not need QEMU to run rustc.
FROM --platform=$BUILDPLATFORM rust:1.98-bookworm AS build
ARG BUILDARCH
ARG TARGETARCH
# The base image already provides the pinned Rust release. Copying the
# rust-toolchain file makes rustup try to sync the toolchain on every build,
# which fails when the builder has no DNS.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gcc_pkg=gcc-x86-64-linux-gnu; gcc=x86_64-linux-gnu-gcc ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gcc_pkg=gcc-aarch64-linux-gnu; gcc=aarch64-linux-gnu-gcc ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /rust-target; \
    : > /cross-env; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "$gcc_pkg" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
      upper=$(echo "$triple" | tr 'a-z-' 'A-Z_'); \
      lower=$(echo "$triple" | tr '-' '_'); \
      printf 'export CARGO_TARGET_%s_LINKER=%s\nexport CC_%s=%s\n' "$upper" "$gcc" "$lower" "$gcc" > /cross-env; \
    fi; \
    rustup target add "$triple"
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG REVISION=unknown
RUN set -eu; \
    . /cross-env; \
    triple=$(cat /rust-target); \
    CHANNELLOOM_REVISION="$REVISION" cargo build --release --locked -p awesometv-server --target "$triple"; \
    cp "target/$triple/release/awesometv-server" /awesometv-server

# `docker buildx build --target binary --output type=local,dest=out` exports
# just the server binary. Release tarballs are built this way.
FROM scratch AS binary
COPY --from=build /awesometv-server /awesometv-server

FROM ubuntu:26.04 AS runtime
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="ChannelLoom" \
      org.opencontainers.image.description="Turn a personal media library into live TV channels for Plex, Jellyfin, Emby and IPTV clients." \
      org.opencontainers.image.source="https://github.com/cjrutherford/channel-loom" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /awesometv-server /usr/local/bin/awesometv-server
# uid/gid 1000 is the image's stock `ubuntu` user. Override with `user:` in
# compose to match the owner of your data and media directories.
RUN install -d -o 1000 -g 1000 /data /media
ENV AWESOMETV_BIND=0.0.0.0 \
    AWESOMETV_PORT=8409 \
    AWESOMETV_DATA_DIR=/data \
    AWESOMETV_MEDIA_ROOT=/media
USER 1000:1000
VOLUME ["/data"]
EXPOSE 8409
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD ["/usr/local/bin/awesometv-server", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/awesometv-server"]

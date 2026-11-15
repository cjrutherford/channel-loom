FROM rust:1.98-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p awesometv-server

FROM ubuntu:26.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/awesometv-server /usr/local/bin/awesometv-server
ENV AWESOMETV_BIND=127.0.0.1 \
    AWESOMETV_PORT=8409 \
    AWESOMETV_DATA_DIR=/data \
    AWESOMETV_MEDIA_ROOT=/media \
    JELLYFIN_URL=https://jelly.christopherrutherford.net
VOLUME ["/data"]
EXPOSE 8409
ENTRYPOINT ["/usr/local/bin/awesometv-server"]

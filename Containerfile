# Multi-stage build: static musl binary on a small, unprivileged runtime image.
#   podman build -t pulp-tui -f Containerfile .
#   podman run --rm -it -v ./config.toml:/config/config.toml:ro,Z -v pulp-tui-data:/data pulp-tui
FROM docker.io/library/rust:1.99-alpine3.22 AS build
# C toolchain for bundled SQLite and aws-lc (rustls crypto provider).
RUN apk add --no-cache musl-dev build-base cmake perl
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml build.rs ./
COPY src src
COPY examples examples
# No system keyring in containers: secrets come from env:VAR or file:/run/secrets/...
# The release build passes CARGO_FLAGS= to keep the keyring in the static binary.
ARG CARGO_FLAGS=--no-default-features
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked $CARGO_FLAGS \
 && cp target/release/pulp-tui /pulp-tui

FROM docker.io/library/alpine:3.24
RUN apk add --no-cache ca-certificates \
 && adduser -D -H -u 10001 pulptui \
 && mkdir -p /data /config && chown 10001 /data
COPY --from=build /pulp-tui /usr/local/bin/pulp-tui
USER 10001
ENV PULP_TUI_CONFIG=/config/config.toml \
    PULP_TUI_DATA_DIR=/data \
    TERM=xterm-256color
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/pulp-tui"]

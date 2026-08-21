# syntax=docker/dockerfile:1

# ------------------------------------------------------------------------------
# Stage 1: Build
# ------------------------------------------------------------------------------

FROM rust:1.97-alpine AS builder

ENV OPENSSL_STATIC=1

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconf cmake make g++ git

WORKDIR /src

# ------------------------------------------------------------------------------
# Cache Build
# ------------------------------------------------------------------------------

# Cache dependency builds: copy only manifests first, then
# create stub source files so `cargo build` resolves and
# compiles all dependencies without the real source code.
# See: https://shaneutt.com/blog/rust-fast-small-docker-image-builds/

COPY Cargo.toml Cargo.lock ./
COPY crates/experimental-probe/Cargo.toml crates/experimental-probe/Cargo.toml
COPY crates/praxis-experimental-filters/Cargo.toml crates/praxis-experimental-filters/Cargo.toml
COPY crates/praxis-experimental-server/Cargo.toml crates/praxis-experimental-server/Cargo.toml

# The server crate's build.rs discovers filter crates via `cargo metadata`
# for build-time auto-registration. Cargo compiles the build script (and its
# dependencies) up front, so build.rs needs its real source here — a stub
# would emit no registration code and silently produce a server with none of
# this workspace's filters.
COPY crates/praxis-experimental-server/build.rs crates/praxis-experimental-server/build.rs

# Every workspace member's manifest must resolve, so stub a
# source file for each (lib crates get lib.rs, bin crates main.rs).
RUN mkdir -p crates/experimental-probe/src \
    && echo '//! stub' > crates/experimental-probe/src/lib.rs \
    && printf '//! stub\nfn main() {}\n' > crates/experimental-probe/src/main.rs \
    && mkdir -p crates/praxis-experimental-filters/src \
    && echo '//! stub' > crates/praxis-experimental-filters/src/lib.rs \
    && mkdir -p crates/praxis-experimental-server/src \
    && printf '//! stub\nfn main() {}\n' > crates/praxis-experimental-server/src/main.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p praxis-experimental-server

# ------------------------------------------------------------------------------
# Cache Tricks
# ------------------------------------------------------------------------------

# Replace stubs with real source, then rebuild. Only the
# project crates recompile; all dependencies are cached.

COPY crates/experimental-probe/src crates/experimental-probe/src
COPY crates/praxis-experimental-filters/src crates/praxis-experimental-filters/src
COPY crates/praxis-experimental-server/src crates/praxis-experimental-server/src

# Touch the source files so cargo sees them as newer than
# the cached stub artifacts.
RUN find crates -name '*.rs' -exec touch {} +

# ------------------------------------------------------------------------------
# Build
# ------------------------------------------------------------------------------

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p praxis-experimental-server \
    && cp target/release/praxis-experimental-server /usr/local/bin/praxis-experimental-server

# ------------------------------------------------------------------------------
# Stage 2: Runtime
# ------------------------------------------------------------------------------

FROM alpine:3.24

LABEL org.opencontainers.image.source="https://github.com/praxis-proxy/experimental" \
    org.opencontainers.image.description="Praxis experimental AI gateway (praxis-ai + experimental filters)" \
    org.opencontainers.image.licenses="MIT"

RUN apk add --no-cache ca-certificates \
    && addgroup -S praxis \
    && adduser -S -G praxis -h /nonexistent -s /sbin/nologin praxis \
    && mkdir -p /etc/praxis

COPY --from=builder --chown=root:root --chmod=0555 \
    /usr/local/bin/praxis-experimental-server /usr/local/bin/praxis-experimental-server

USER praxis:praxis

WORKDIR /etc/praxis

EXPOSE 8080 9901

HEALTHCHECK --interval=5s --timeout=3s --start-period=2s \
    CMD wget -qO- http://127.0.0.1:9901/healthy || exit 1

ENTRYPOINT ["praxis-experimental-server"]

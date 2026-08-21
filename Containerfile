# syntax=docker/dockerfile:1

# ------------------------------------------------------------------------------
# Stage 1: Build
# ------------------------------------------------------------------------------

FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev

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
COPY crates/switchyard-filters/Cargo.toml crates/switchyard-filters/Cargo.toml
COPY crates/switchyard-server/Cargo.toml crates/switchyard-server/Cargo.toml

# Every workspace member's manifest must resolve, so stub a
# source file for each (lib crates get lib.rs, bin crates main.rs).
RUN mkdir -p crates/experimental-probe/src \
    && echo '//! stub' > crates/experimental-probe/src/lib.rs \
    && printf '//! stub\nfn main() {}\n' > crates/experimental-probe/src/main.rs \
    && mkdir -p crates/switchyard-filters/src \
    && echo '//! stub' > crates/switchyard-filters/src/lib.rs \
    && mkdir -p crates/switchyard-server/src \
    && printf '//! stub\nfn main() {}\n' > crates/switchyard-server/src/main.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p experimental-probe

# ------------------------------------------------------------------------------
# Cache Tricks
# ------------------------------------------------------------------------------

# Replace stubs with real source, then rebuild. Only the
# project crates recompile; all dependencies are cached.

COPY crates/experimental-probe/src crates/experimental-probe/src

# Touch the source files so cargo sees them as newer than
# the cached stub artifacts.
RUN find crates -name '*.rs' -exec touch {} +

# ------------------------------------------------------------------------------
# Build
# ------------------------------------------------------------------------------

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p experimental-probe \
    && cp target/release/experimental-probe /usr/local/bin/experimental-probe

# ------------------------------------------------------------------------------
# Stage 2: Runtime
# ------------------------------------------------------------------------------

FROM alpine:3.23

LABEL org.opencontainers.image.source="https://github.com/praxis-proxy/experimental" \
    org.opencontainers.image.description="Praxis experimental probe binary" \
    org.opencontainers.image.licenses="MIT"

RUN apk add --no-cache ca-certificates \
    && addgroup -S probe \
    && adduser -S -G probe -h /nonexistent -s /sbin/nologin probe

COPY --from=builder --chown=root:root --chmod=0555 \
    /usr/local/bin/experimental-probe /usr/local/bin/experimental-probe

USER probe:probe

# When scaffolding a long-running service, add EXPOSE and a HEALTHCHECK
# here and update the container workflow to wait for healthy status.

ENTRYPOINT ["experimental-probe"]

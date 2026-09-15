# syntax=docker/dockerfile:1

# Red Hat UBI9 image for the experimental Praxis AI gateway.
#
# Base images are UBI rather than Alpine because this image is the upstream of
# the Open Data Hub midstream build: Konflux's base-image trust policy and the
# Red Hat container catalogue both expect UBI, and building the same file here
# and there means GHCR and quay.io ship identical bits.
#
# The Rust toolchain comes from the checksum-pinned upstream release tarball
# rather than from RHEL's AppStream `rust-toolset`, which is 1.92 and so cannot
# satisfy this workspace's `rust-version = "1.96"`. This is the same mechanism
# opendatahub-io/openshell uses; under a hermetic Konflux build the identical
# tarball is prefetched to /cachi2/output/deps/generic, pinned by URL and
# checksum in that pipeline's generic-fetcher lockfile, and the RUN below picks
# it up from there instead of reaching the network. Nothing in this repo needs
# to change to enable that.

# ------------------------------------------------------------------------------
# Stage 1: Build
# ------------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi:9.8@sha256:25a147defd01e19674714f55d17538c8dbe55d8c305fa157ecc3f9c8977b05b6 AS builder

# No openssl-devel: the binary links rustls, so nothing in the graph builds
# against system OpenSSL. Verified with ldd on the produced binary.
RUN dnf install -y --nodocs --setopt=install_weak_deps=0 \
        gcc cmake make xz \
    && dnf clean all

# Hermeto's cargo prefetch vendors against the sparse index; matching the
# protocol here keeps a later hermetic build byte-identical to this one.
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
    PATH=/usr/local/bin:$PATH

# Keep in lockstep with rust-toolchain.toml. Both checksums are the official
# ones published alongside the tarballs at static.rust-lang.org.
ARG RUST_VERSION=1.96.1
ARG RUST_SHA256_X86_64=d29ccb1559a177c4e72291f6e5f629de7fe8885e7521ca47802627544b121e95
ARG RUST_SHA256_AARCH64=3abcb9489d001d95f30e8cfe68118be85afb0adbf0a9b21438909719689c08fb

RUN set -eu; \
    case "$(uname -m)" in \
      x86_64)  triple=x86_64-unknown-linux-gnu;  sha="${RUST_SHA256_X86_64}" ;; \
      aarch64) triple=aarch64-unknown-linux-gnu; sha="${RUST_SHA256_AARCH64}" ;; \
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    tarball="rust-${RUST_VERSION}-${triple}.tar.xz"; \
    prefetched="/cachi2/output/deps/generic/${tarball}"; \
    if [ -f "${prefetched}" ]; then \
      cp "${prefetched}" /tmp/rust.tar.xz; \
    else \
      curl -fsSL -o /tmp/rust.tar.xz "https://static.rust-lang.org/dist/${tarball}"; \
    fi; \
    printf '%s  /tmp/rust.tar.xz\n' "${sha}" | sha256sum -c -; \
    mkdir -p /tmp/rust; \
    tar xJf /tmp/rust.tar.xz -C /tmp/rust --strip-components=1; \
    /tmp/rust/install.sh --prefix=/usr/local \
        --components="rustc,cargo,rust-std-${triple}"; \
    rm -rf /tmp/rust /tmp/rust.tar.xz; \
    rustc --version; cargo --version

WORKDIR /src

ARG FEATURES=""

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

# Konflux mounts an ephemeral container store and passes --no-cache, so these
# mounts help local and GitHub Actions builds only; they are inert there.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p praxis-experimental-server ${FEATURES:+--features "$FEATURES"}

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

RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p praxis-experimental-server ${FEATURES:+--features "$FEATURES"} \
    && install -m 0555 target/release/praxis-experimental-server \
       /usr/local/bin/praxis-experimental-server

# ------------------------------------------------------------------------------
# Stage 2: Runtime
# ------------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi-minimal:9.8@sha256:7fbeae18dc9476399f565e68255f602a3374ea8614ba3d14843565131a13ff93

# Re-declare in this stage: ARG scope does not cross FROM boundaries.
ARG FEATURES=""

# Overridable so the midstream build can stamp product values (rhoai/... name,
# Red Hat vendor, release) without needing a second Containerfile.
ARG IMAGE_NAME="praxis-experimental"
ARG VENDOR="Praxis Contributors"
ARG VERSION="0.0.0"
ARG RELEASE="1"

# name/vendor/version/release/summary/description/maintainer are the labels
# Red Hat container certification requires; the rest are catalogue UX.
#
# io.praxis.build.features records which non-default cargo features the binary
# was compiled with, so a pulled image can be interrogated for its capabilities:
#   docker inspect --format \
#     '{{index .Config.Labels "io.praxis.build.features"}}' <image>
LABEL org.opencontainers.image.source="https://github.com/praxis-proxy/experimental" \
    org.opencontainers.image.description="Praxis experimental AI gateway (praxis-ai + experimental filters)" \
    org.opencontainers.image.licenses="Apache-2.0" \
    io.praxis.build.features="${FEATURES}" \
    name="${IMAGE_NAME}" \
    vendor="${VENDOR}" \
    version="${VERSION}" \
    release="${RELEASE}" \
    summary="Praxis experimental AI gateway" \
    description="OpenAI- and Anthropic-compatible AI gateway built on Praxis, with experimental filters" \
    maintainer="https://github.com/praxis-proxy/experimental" \
    io.k8s.display-name="Praxis experimental AI gateway" \
    io.k8s.description="OpenAI- and Anthropic-compatible AI gateway built on Praxis, with experimental filters" \
    io.openshift.tags="ai,gateway,llm,proxy" \
    io.openshift.expose-services="8080:http,9901:http"

# No package installs: the pinned ubi-minimal already ships curl (which backs the
# HEALTHCHECK below) and ca-certificates. Installing them explicitly is a no-op
# that only costs three metadata fetches, and the digest pin means the base
# cannot drop them without a deliberate bump -- re-check both if that bump happens.
RUN mkdir -p /etc/praxis /licenses

COPY LICENSE /licenses/LICENSE

COPY --from=builder --chown=root:root --chmod=0555 \
    /usr/local/bin/praxis-experimental-server /usr/local/bin/praxis-experimental-server

# Assets for the ai-gateway DEMO, so trying it needs no repository checkout.
# These are not the image's configuration -- see the note below.
#
# The compose file here seeds the rest of them into volumes that the demo's
# observability services mount, which is what removes the checkout and also means
# the dashboards can never drift from the binary they describe. Fetch it with the
# raw URL, or straight out of the image when offline:
#
#   id=$(podman create <image>)
#   podman cp "$id:/usr/share/praxis/demo/compose.yaml" compose.yaml
#   podman rm "$id"
#
# See demos/ai-gateway/docs/quickstart.md.
#
# Exactly what the compose path needs and nothing else, about 130 KB. That is
# small enough to keep in this image rather than publish a second one: a separate
# demo image would have to be kept in step with this binary, and the version-lock
# is the whole point. Named file by file rather than by directory, because a
# directory copy also picks up whatever a developer left in their tree: a
# gitignored .env holding a real API key, editor state, and the KIND-only
# datasources this path cannot use.
#
# NOTE ON THE IMAGE'S OWN CONFIG. /etc/praxis is deliberately left empty, so a
# bare `podman run` warns and starts on built-in defaults -- loopback listeners
# and no filters. The product's sample config, examples/configs/gateway.yaml, is
# NOT shipped here and none of the files below are a default. Making gateway.yaml
# the image default is real and wanted, but it is a product decision rather than a
# demo one: it also needs a default upstream that exists, since that file points
# at 127.0.0.1:3000 and would answer 502 out of the box.
COPY demos/ai-gateway/configs/ \
    /usr/share/praxis/demo/configs/
COPY demos/ai-gateway/compose/compose.quick.yaml \
    /usr/share/praxis/demo/compose.yaml
COPY demos/ai-gateway/compose/tempo.yaml \
    demos/ai-gateway/compose/prometheus.yaml \
    demos/ai-gateway/compose/otel-collector.yaml \
    /usr/share/praxis/demo/stack/
COPY demos/ai-gateway/observability/perses/config.yaml \
    demos/ai-gateway/observability/perses/project.json \
    /usr/share/praxis/demo/perses/
COPY demos/ai-gateway/observability/perses/dashboards/ \
    /usr/share/praxis/demo/perses/dashboards/
COPY demos/ai-gateway/observability/perses/datasources/compose/ \
    /usr/share/praxis/demo/perses/datasources/

# Numeric UID with no /etc/passwd entry: OpenShift's restricted SCC assigns a
# UID from the namespace's range regardless of what USER says, and keeps GID 0.
USER 1001

# The server resolves its configuration from ./praxis.yaml when --config is not
# given, so the working directory is the mount point for a config file.
WORKDIR /etc/praxis

EXPOSE 8080 9901

HEALTHCHECK --interval=5s --timeout=3s --start-period=2s \
    CMD curl --fail --silent --show-error http://127.0.0.1:9901/healthy || exit 1

ENTRYPOINT ["praxis-experimental-server"]

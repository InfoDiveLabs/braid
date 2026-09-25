# syntax=docker/dockerfile:1

# ---- build ------------------------------------------------------------
#
# rust-toolchain.toml at the repo root pins the "stable" channel, not one
# numbered release, so there is no version number to hardcode here without
# guessing at one the repository itself never commits to. The official rust
# image already tracks that channel, and copying the toolchain file into the
# build lets rustup confirm that rather than this Dockerfile assuming it.
FROM rust:slim-bookworm AS builder

# rustls resolves to aws-lc-rs on this workspace, and aws-lc-sys compiles its
# own C code with cmake and a C compiler. The slim base has neither, and the
# failure without them surfaces as a linker error inside a dependency three
# levels down, not as an obvious "apt-get install cmake".
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY rust-toolchain.toml ./
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY xtask xtask

# Cache mounts rather than a manifests-then-source layering trick: this
# workspace has eight member crates, and the usual dummy-source version of
# that trick means hand-maintaining eight fake main.rs/lib.rs files that
# silently go stale the day a crate gains a new one. A cache mount keyed on
# the target and registry directories gets the same result, a source edit no
# longer forcing a rebuild of librqbit and everything else dl-server pulls
# in, without a second copy of the crate list to keep in sync with the real
# one. Only dl-server is asked for: dl-gui is a Slint desktop application
# whose windowing libraries have no business in a headless image, and its
# source sits alongside dl-server's in the workspace only because cargo
# needs every member's manifest to resolve one, not because it gets built.
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release -p dl-server \
    && cp target/release/braid-server /usr/local/bin/braid-server

# ---- runtime ------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates for the HTTPS trackers and HTTP sources Braid connects to
# outbound. wget is here only for the HEALTHCHECK below: braid-server has no
# self-check flag of its own yet, and wget is the smallest thing on this base
# that can poll an HTTP endpoint. curl would pull libcurl and, behind it,
# ldap/ssh2/kerberos: a dozen packages against wget's one.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        wget \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/braid-server /usr/local/bin/braid-server
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

VOLUME ["/config", "/downloads"]

# 8080 for the web UI and both APIs. 6881 for BitTorrent, TCP and UDP both:
# UDP is not the optional half, it is what DHT and UDP trackers use to find
# peers, and a client missing it just finds fewer of them for no visible
# reason.
EXPOSE 8080
EXPOSE 6881/tcp
EXPOSE 6881/udp

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD wget -q -O /dev/null "http://127.0.0.1:${BRAID_WEB_PORT:-8080}/health" || exit 1

ENTRYPOINT ["/entrypoint.sh"]
CMD ["braid-server"]

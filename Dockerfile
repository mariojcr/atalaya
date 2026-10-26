# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0
# Both images from one build of the workspace:
#
#   docker build --target server -t atalaya-server .
#   docker build --target collector -t atalaya-collector .
#
# The Rust version is the one in rust-toolchain.toml; bump both together. Base
# images are pinned by the digest of their multi-architecture index (the tag is
# kept for reading); Dependabot proposes new digests (.github/dependabot.yml).
FROM docker.io/library/rust:1.99.0-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY collector collector
COPY server server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && mkdir /out && cp target/release/atalaya-server target/release/atalaya-collector /out/

FROM docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS server
# The server needs no privileges: it runs as atalaya (uid and gid 10001) and
# keeps notebooks, saved searches and alert state in /var/lib/atalaya, which
# that user owns. A new named volume mounted there inherits the ownership; one
# created by an earlier image as root needs a chown (docs/deployment.md).
RUN groupadd --gid 10001 atalaya \
 && useradd --uid 10001 --gid atalaya --no-log-init --no-create-home --home-dir /var/lib/atalaya --shell /usr/sbin/nologin atalaya \
 && install -d -o atalaya -g atalaya -m 0750 /var/lib/atalaya
COPY --from=build /out/atalaya-server /usr/local/bin/atalaya-server
COPY LICENSE /usr/share/doc/atalaya/
# Configuration (dashboards, overview, timeline and alerts) is not part of the
# image: ATALAYA_DASHBOARDS points to it
ENV ATALAYA_DATA=/var/lib/atalaya
WORKDIR /var/lib/atalaya
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/atalaya-server"]

FROM docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS collector
# The collector runs as root: it reads the host's logs and hardware (sysfs,
# Nomad allocation logs, Docker logs) and runs smartctl for disk health. See
# docs/collector.md#privileges.
RUN apt-get update \
 && apt-get install -y --no-install-recommends smartmontools \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/atalaya-collector /usr/local/bin/atalaya-collector
COPY LICENSE /usr/share/doc/atalaya/
ENTRYPOINT ["/usr/local/bin/atalaya-collector"]

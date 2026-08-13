# syntax=docker/dockerfile:1.7

# ── Stage 1: builder ─────────────────────────────────────────────────────────
# Tag SANS version Rust : l'image ne fournit que rustup + la libc. La toolchain
# effectivement utilisée est celle de rust-toolchain.toml, copié ci-dessous —
# source unique du pin (invariant toolchain, council Art.19 2026-07-24).
# Avant ce changement l'image était figée sur rust:1.95.0 : les binaires du
# conteneur étaient compilés par un compilateur DIFFÉRENT de celui qui les
# valide en CI (rust-toolchain.toml n'était pas copié dans le contexte de build).
FROM rust:slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Toolchain épinglée AVANT les sources : couche cachée indépendamment du code.
COPY rust-toolchain.toml ./
RUN rustup toolchain install --no-self-update && rustc --version

# Copie des sources
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --workspace \
    --bin gradatum-server --bin gradatum-worker \
    --bin gradatum-admin \
    --bin gradatum-gateway \
    --bin gradatum-engine --features gradatum-engine/serve

# ── Stage 2: runtime ─────────────────────────────────────────────────────────
FROM debian:13-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 sqlite3 curl \
    && rm -rf /var/lib/apt/lists/*

# Utilisateur gradatum UID 991
# Caveat Gate1-BLQ : UID 990 réservé à systemd-timesync — utiliser 991
RUN groupadd -g 991 gradatum \
 && useradd -u 991 -g 991 -m -s /sbin/nologin gradatum

COPY --from=builder /build/target/release/gradatum-server  /usr/local/bin/
COPY --from=builder /build/target/release/gradatum-worker  /usr/local/bin/
COPY --from=builder /build/target/release/gradatum-admin   /usr/local/bin/
COPY --from=builder /build/target/release/gradatum-gateway /usr/local/bin/
COPY --from=builder /build/target/release/gradatum-engine  /usr/local/bin/

# Répertoires d'état : ConfigDir + StateDir + LogDir
RUN mkdir -p /etc/gradatum /var/lib/gradatum /var/log/gradatum \
 && chown gradatum:gradatum /etc/gradatum /var/lib/gradatum /var/log/gradatum

USER gradatum
WORKDIR /var/lib/gradatum

EXPOSE 19090

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS http://127.0.0.1:19090/health || exit 1

ENTRYPOINT ["/usr/local/bin/gradatum-server"]
CMD ["--config", "/etc/gradatum/server.toml"]

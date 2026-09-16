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

# `git` : six build.rs du workspace (dont ceux des 5 binaires compilés ici)
# résolvent le SHA court du commit par `git rev-parse --short HEAD` et retombent
# SILENCIEUSEMENT sur `unknown` si la commande échoue — sans ce paquet, toute image
# publiée annoncerait une provenance (`build_sha` dans /health) qui ne prouve rien.
# Absent de rust:slim-bookworm. Le `rm -rf` final ne laisse aucun cache apt.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

# Toolchain épinglée AVANT les sources : couche cachée indépendamment du code.
COPY rust-toolchain.toml ./
RUN rustup toolchain install --no-self-update && rustc --version

# Copie des sources
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Dépôt git copié APRÈS les sources et JUSTE AVANT la compilation : seule cette
# dernière couche devient sensible au commit construit, tout ce qui précède
# (toolchain, apt, manifestes, sources) garde son cache. L'arbre entier est copié,
# pas un sous-ensemble `HEAD`/`refs` : `--short` résout l'abréviation dans la base
# d'objets, qui doit donc être là — un sous-ensemble ferait échouer la résolution
# en silence, soit exactement le défaut corrigé ici. Ce stage est discardé : le
# stage runtime ne reprend que les 5 binaires ci-dessous, jamais ce répertoire.
COPY .git ./.git

RUN cargo build --release --workspace \
    --bin gradatum-server --bin gradatum-worker \
    --bin gradatum-admin \
    --bin gradatum-gateway \
    --bin gradatum-engine --features gradatum-engine/serve

# ── Stage 2: runtime ─────────────────────────────────────────────────────────
FROM debian:13-slim

# sqlite3 : gardé pour l'exploitation — inspection à chaud des bases depuis le
# conteneur. Il n'est PAS sur le chemin d'exécution des binaires gradatum, qui lient
# SQLite en bibliothèque (rusqlite/libsqlite3-sys) et n'invoquent jamais le CLI.
# Retrait prévu en 3.0.0 ; pas purgé ici — la série 2.x ne retire pas un outil
# d'exploitation en cours de vie.
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

# 90 s, par la mesure et non par précaution : sur l'hôte de référence le premier
# /health vert tombe ~40 s après le boot, donc une fenêtre de 10 s déclarait en
# mauvaise santé un serveur sain (faux rouge à chaque démarrage). 90 s garde la marge
# sur cette mesure et dépasse le plancher de 60 s du harnais de montée ; au-delà, un
# démarrage lent est un signal réel. Vaut aussi hors compose : l'image porte son
# propre HEALTHCHECK, qui s'applique quand le conteneur est lancé sans ce fichier.
HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
  CMD curl -fsS http://127.0.0.1:19090/health || exit 1

ENTRYPOINT ["/usr/local/bin/gradatum-server"]
CMD ["--config", "/etc/gradatum/server.toml"]

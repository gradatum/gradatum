# gradatum-admin

> Operator CLI for gradatum: init, token, api-key, backfill, jobs, vault rename/forget.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-admin` is the operator-facing CLI. It handles setup, maintenance, and lifecycle
operations that run outside the HTTP API — typically as root or the service user.

## Subcommands

### init

Bootstrap the gradatum data directory.

```bash
gradatum-admin init --preset hierarchical --root /var/lib/gradatum
gradatum-admin init --root /var/lib/gradatum --force   # re-init
```

Generates: the admin bearer token, `server.toml`, the ACL preset, and the SQLite
databases (`queue`, `revocation`, `api_keys`).

`init` does **not** create the JWT signing key: `gradatum-server` generates
`config/jwt-signing-key.secret` at first boot and is its sole owner.

### token

Manage service JWT tokens.

```bash
gradatum-admin token issue --root /var/lib/gradatum --sub my-service --scopes vault_read
```

### api-key

Create and manage API keys for consumers.

```bash
gradatum-admin api-key create --root /var/lib/gradatum --owner agent-1 --scopes write
gradatum-admin api-key create --root /var/lib/gradatum --owner reader-1 --scopes vault_read --read-only
gradatum-admin api-key list   --root /var/lib/gradatum
gradatum-admin api-key revoke --root /var/lib/gradatum --prefix ak_abcdef01
gradatum-admin api-key rotate --root /var/lib/gradatum --prefix ak_abcdef01
```

Write access is granted only by the scopes `write`, `admin` and `service`. `create`
refuses any other scope set unless `--read-only` is passed, so it never mints a key
whose scopes promise write access the key does not have. The check runs on `create`
only: `rotate` reuses the source key's scopes as-is without revalidating them, and
keys already in the store are left untouched.

### backfill-embeddings

Backfill embeddings for notes without an entry in `note_embeddings` (idempotent).

```bash
gradatum-admin backfill-embeddings --root /var/lib/gradatum [--tenant main] [--limit 100]
```

### backfill-titles

Backfill missing titles (WHERE title IS NULL) from H1 Markdown headers (idempotent).

```bash
gradatum-admin backfill-titles --root /var/lib/gradatum [--tenant main] [--dry-run] [--limit N]
```

### jobs

Introspect and manage the job queue.

```bash
gradatum-admin jobs list   --root /var/lib/gradatum [--status pending] [--kind Curate] [--limit 50]
gradatum-admin jobs get    --root /var/lib/gradatum <id>
gradatum-admin jobs cancel --root /var/lib/gradatum <id>
gradatum-admin jobs dlq    --root /var/lib/gradatum [--replay <id>] [--replay-all]
```

### vault rename

Rename a note — updates `notes.title` in the index and registers a redirect.

```bash
gradatum-admin vault rename "Old Title" "New Title" --root /var/lib/gradatum [--tenant main]
```

### vault forget

Semantic forget of a batch of notes. Double-confirmation workflow.

```bash
# Step 1 — preview (dry-run, default)
gradatum-admin vault forget topic --query "projet X" --root /var/lib/gradatum

# Step 2 — execute with confirmed ULIDs
gradatum-admin vault forget topic --query "projet X" \
    --execute --confirm-ulids "01J...,01J..." --root /var/lib/gradatum
```

Scopes: `topic` (FTS query) · `locus` (path prefix) · `agent` (agent_id).

## License

Apache-2.0

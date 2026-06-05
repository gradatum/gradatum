# gradatum-admin

> Operator CLI for Gradatum: bootstrap, migration, backup/restore, and vault lifecycle management.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Subcommands

### init

Bootstrap a Gradatum root directory (Phase 2.0a).

```
gradatum-admin init --preset hierarchical --root /var/lib/gradatum
gradatum-admin init --root /var/lib/gradatum --force   # re-init
```

Generates:
- `jwt_ed25519.key` / `jwt_ed25519.pub` (Ed25519 keypair, chmod 600/644)
- `admin_bearer.txt` (auto-generated admin token, chmod 600)
- `config.toml` (default server configuration)
- `queue.db` (SQLite queue)
- `acl/hierarchical.toml` (ACL preset)

### vault

```
gradatum-admin vault create <name>
gradatum-admin vault list
gradatum-admin vault swap <from> <to>
gradatum-admin vault delete <name> [--confirm]
```

### migrate

```
gradatum-admin migrate --from v0.x --to v0.1 --root /var/lib/gradatum
```

### backup / restore

```
gradatum-admin backup --root /var/lib/gradatum --output /backup/gradatum-$(date +%Y%m%d).tar.gz
gradatum-admin restore --input /backup/gradatum-20260504.tar.gz --root /var/lib/gradatum
```

## ACL Presets

| Preset | Description |
|---|---|
| `hierarchical` | Recommended — section-based RBAC with personal-classified guard |
| `open` | All authenticated consumers have read access (no write by default) |
| `strict` | Explicit whitelist per consumer per section |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation → `v0.1.0` public
- License : Apache-2.0
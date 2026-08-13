# Guide D — MCP integration & Studio

Assumes gradatum is already installed and `gradatum-server` is running — see
[Guide A](A-docker-quickstart.md), [Guide B](B-install-binaries.md), or
[Guide C](C-build-from-source.md).

---

## MCP integration

Gradatum exposes its memory surface over MCP. An MCP client authenticates with a long-lived API
key carrying a write scope. For an MCP client such as Claude Code, `main-agent` ("Orchestrator —
any LLM-based agent") is the fitting identity — and **that key already exists on any install
initialised with `gradatum-admin init`**: `init` mints the mandatory `main-agent` bootstrap key
(R4) with scopes `vault_read,vault_search,vault_write,write` and records its secret in
`config/main-agent.apikey.txt` (mode 0600) under `--root`. Read it from there rather than
creating a new one — `api-key create --owner main-agent` is *refused* on an initialised root (see
"One identity, one active key" below). Read the file as the `gradatum` service account (or
whatever `--user` the install used), which owns the `0600` file:

```bash
sudo -u gradatum cat /var/lib/gradatum/config/main-agent.apikey.txt
# -> ak_...  Store it with mode 600 where your MCP client reads it, e.g. ~/.config/gradatum/api-key
```

If that secret file was lost, reissue the key with `rotate` (revoke + replace, atomic) — never
`create`, which R1 refuses (below):

```bash
# Find the main-agent key's prefix, then rotate it to print a fresh secret once.
sudo -u gradatum gradatum-admin api-key list --root /var/lib/gradatum
sudo -u gradatum gradatum-admin api-key rotate --prefix <main-agent-prefix> --root /var/lib/gradatum
```

**One identity, one active key.** `api-key create` enforces this at creation time: it refuses a
second active key for an `--owner` that already carries one (`refusing to create a second active
key for '<owner>': an active key already exists`), and the refusal names the existing prefix and
the `rotate` command. Because `init` already minted `main-agent`, `api-key create --owner
main-agent` hits exactly this refusal on a fresh install — `rotate` is the way to reissue that
identity's key. To mint a key for a *different* identity the preset declares (`admin`, `studio`,
`expert-*`, `engine`, `agent-wrapper`, `third-party-agent`), use `api-key create --owner
<that-identity>` — run it as the `gradatum` service account (the vault root defaults to mode
`0750` owned by that account, so a plain non-`sudo` invocation fails to read the ACL preset at
`--root`), and `--owner` must match an identity the preset already declares, or the command is
refused with `refusing to create a key for an undeclared identity`, not silently accepted.

> **The write scopes are a closed set: `write`, `admin`, `service`.** With `multi_tenant.enabled = true`, every write path requires the key to carry at least one of those three, matched by exact string equality (`WRITE_SCOPES`, `gradatum-acl-auth`). Any other value — including `vault_write` — yields a read-only key that takes `403 write scope required (read-only token)` on every write. With `multi_tenant.enabled = false` (the default) scopes are not checked at all, so the same key writes fine until multi-tenant mode is turned on. `api-key create` enforces the same set at creation time: a scope set that grants no write access is **refused**, unless you pass `--read-only` to confirm a read-only key is intended. The check covers creation only — `api-key rotate` carries the source key's scopes over unchanged, and keys minted before this release are not revalidated, so an existing key may still name a scope that grants nothing.
>
> Read access is not governed by key scopes in either mode — it is governed by vault grants and the locus ACL.

`gradatum-server` serves MCP directly over Streamable HTTP at `/mcp` — no separate bridge
process, no separate binary. Point an HTTP-capable MCP client at it with the API key as a
Bearer credential (no token-refresh needed; the key is long-lived):

```json
{
  "mcpServers": {
    "gradatum": {
      "type": "http",
      "url": "http://127.0.0.1:19090/mcp",
      "headers": {
        "Authorization": "Bearer ak_your_api_key"
      }
    }
  }
}
```

This works with any MCP host that lets you attach a custom request header — Claude Code does.
**Claude Desktop does not**: it only takes a URL and drives its own auth flow, with no way to
set a custom `Authorization` header, and gradatum implements no OAuth. Claude Desktop cannot
connect to gradatum until one of the two changes.

---

## Studio login & API-key lifecycle

Gradatum ships a web-based Studio UI at `/ui/` (`http://<host>:19090/ui/`). Authentication uses
an **API key** (`ak_…`).

> **The Studio UI is not shipped in any pre-built binary or archive.** It is served from a
> static bundle on disk (`crates/gradatum-server/src/studio.rs`, `ServeDir`/`ServeFile`), and
> that bundle directory is not part of the release archives, the Docker image build, or
> `cargo build`. A deployment built from [Guide B](B-install-binaries.md) or
> [Guide C](C-build-from-source.md) will have `gradatum-server` running with `/ui/*` returning
> nothing to serve, until the Studio bundle is built and placed on disk separately. This is a
> product gap, not a documentation gap — noted here rather than silently assumed.

**Login flow**

1. Enter the API key on the Studio login screen.
2. The browser posts it to `POST /auth/exchange` and receives a **JWT** stored in
   `localStorage`. The original key is not retained after the exchange.
3. Sessions expire after **1 hour**; a new login (key → JWT exchange) is required after expiry.
4. **Any valid (non-revoked) API key logs in.** `POST /auth/exchange` verifies the key's
   Argon2id hash and the tenant allow-list — it does **not** inspect the key's scopes. The
   `scope: "human"` the Studio sends selects the **token TTL** (1 h), not a permission level.

> **`admin` is a recommended convention, not an enforced requirement.** The key's scopes are
> copied into the JWT and checked only on **write** paths, and only when
> `multi_tenant.enabled = true` — where any of `write`, `admin` or `service` is accepted
> (`WRITE_SCOPES`). With `multi_tenant.enabled = false` (the default), no scope is checked at
> all: a key with an empty or arbitrary scope list has the same access as an `admin` key.
> Grant `admin` so that access stays correct if you later enable multi-tenancy.

**Create a key for the Studio**

Unlike `main-agent`, `init` does **not** mint a `studio` key — so here you *do* create one, under
the same two constraints noted above: run as the `gradatum` service account, and use an `--owner`
the ACL preset declares — `studio` ("Studio UI — full vault read/write, sees personal-classified")
is the one the bundled `hierarchical` preset ships for this purpose:

```bash
sudo -u gradatum gradatum-admin api-key create \
  --root /var/lib/gradatum \
  --owner studio \
  --scopes admin \
  --tenant main \
  --description "studio login"
# Prints the secret once (ak_...). Store it securely — it cannot be retrieved later.
```

> **Secret is shown once.** The key is stored as an Argon2id hash; the plaintext `ak_…` value
> is never retrievable after creation. If the value is lost, use `rotate`.

**Manage keys**

```bash
# List all keys — shows prefix, owner, scopes, and status (never the secret)
gradatum-admin api-key list --root /var/lib/gradatum

# Rotate a key — revokes the current one and prints a new secret once
gradatum-admin api-key rotate --prefix <prefix> --root /var/lib/gradatum

# Revoke a key — blocks new token issuance from it
gradatum-admin api-key revoke --prefix <prefix> --root /var/lib/gradatum
```

**Lost your key?** Run `list` to find the prefix, then `rotate --prefix <prefix>` to get a new
secret.

> **Revocation is not immediate for tokens already issued.** `revoke` and `rotate` stop a key
> being exchanged for new JWTs, but a JWT minted before the revocation stays valid until it
> expires (1 h for `human` scope, 24 h for service scope). To cut every outstanding token at
> once, rotate the signing seed as described in [SECURITY.md](../../SECURITY.md).

---

## Agent skills

Gradatum ships an optional companion plugin,
**[gradatum-skills](https://github.com/gradatum/gradatum-skills)** (Apache-2.0, separate
repository). It packages **10 skills** that teach an agent harness *when* to reach for the
vault and *which* MCP tool to call — a search-before-write discipline, section routing,
just-in-time lesson recall, and a code-navigation path over the derived code index.

The skills contain no script, no binary and no local dependency: each one names an MCP tool of
the `gradatum` server and the harness performs the call. Transport, authentication and response
format belong to the server, not to the plugin. See that repository's `README.md` and
`ARCHITECTURE.md` for the skill catalogue and the L1 → L0 composition model.

**Server requirement.** The skills name MCP tools by their exact names, including `job_status`,
which the write path polls to resolve an asynchronous write. A server older than **v1.0.0** does
not expose it: `0.7.6` predates the tool, and a skill naming a tool the server does not expose
fails at call time rather than at install time. Install the skills against a `v1.0.0` or later
server.

**Install by syncing from a committed reference — never by symlinking the clone.** A symlink
turns every edit in the repository into an immediate production change, removing the window in
which a commit or a review can happen. The canonical procedure is the `SYNC-INSTALL` block under
*Installation* in the [gradatum-skills `README.md`](https://github.com/gradatum/gradatum-skills);
it is executed verbatim by that repository's test suite, which is why it lives there and is not
duplicated here. It needs only `git` and `tar`, is idempotent, and removes skills that left the
product while leaving unrelated skills untouched.

**Verify that the installation is operational, not merely present.** The plugin ships two
independent checks — one asserting that every installed skill matches the committed reference,
the other asserting that every MCP tool named by a skill actually exists in the list the server
exposes. The second catches the failure the first cannot see: a correctly installed skill that
names a tool your server does not have. Both are documented under
*Vérifier que l'installation est opérationnelle* in the plugin's `README.md`.

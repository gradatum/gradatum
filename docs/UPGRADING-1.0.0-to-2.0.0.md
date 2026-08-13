# Upgrading from 1.0.0 to 2.0.0

**Read this before upgrading.** 2.0.0 changes how the server determines *who is calling*.
Most installations need one command. Some need one command and a configuration edit.
An installation that never created an API key will stop serving requests until it does.

This guide covers the identity migration only. 2.0.0 also ships an optional S3-compatible
`[storage]` backend (off by default) that writes note bodies in plaintext, with no encryption
applied by gradatum — see [SECURITY.md § Privacy posture](../SECURITY.md#privacy-posture)
before enabling it.

---

## What changed, in one paragraph

Before 2.0.0, a request whose caller could not be resolved was still served: the server fell
back to a default agent identity, and a client could also declare its own identity through the
`X-Gradatum-Agent` request header or an explicit `author` field. From 2.0.0, **the identity of a
caller is the owner of the API key it presents, and nothing else**. There is no default identity,
no client-declared identity, and no silent fallback. A request that cannot be attributed is
refused.

This closes a real gap: a declared header is not an identity. Any holder of a key could write
whatever name they liked into it, and notes were attributed accordingly.

---

## Does this affect you?

Run this against your installation **before** upgrading. It answers the only question that matters:
does at least one active API key exist?

```bash
gradatum-admin api-key list --root <ROOT>
```

| What you see | Impact | What to do |
|---|---|---|
| At least one active key, and your clients already authenticate with it | **None.** Attribution may change for clients that relied on the header (see below) | Read "Attribution changes" |
| Active keys exist, but some clients send only `X-Gradatum-Agent` without a matching key | **Those clients stop being attributed to the declared name** | Give each client its own key |
| No active key at all | **Breaking.** Every request is refused after the upgrade | Create the bootstrap key (below) |
| The command fails or the store is missing | Your root is not initialised | Run `gradatum-admin init --root <ROOT>` |

---

## Required: the bootstrap identity

2.0.0 requires the `main-agent` identity to hold an active key. Without it, the server has no
identity to serve. On an initialised **multi-tenant** server (`multi_tenant.enabled = true`) it
answers **503** with an actionable initialization error naming this bootstrap key; in the default
legacy mode it answers a plain **401**. Either way the fix is the same: mint the key.

`gradatum-admin init` now mints this key as part of initialising a root, so **new installations
need no extra step**. Existing roots created before 2.0.0 may not have one:

```bash
gradatum-admin api-key create \
  --root <ROOT> \
  --owner main-agent \
  --scopes vault_read,vault_search,vault_write,write
```

The key is printed **once**. Store it, then place it in the `Authorization` header of every client
that should act as this identity.

If the command refuses the owner, the identity is not declared in your ACL preset. Add a
`[[consumer]]` block with `identity = "main-agent"` to the preset at `<ROOT>/config/bearer.toml`
and restart the server. The default `hierarchical` preset already declares it.

---

## Managing other agents' souls: the `identity_write` scope

The bootstrap key above is minted with exactly four scopes — `vault_read`, `vault_search`,
`vault_write`, `write` — and **not** `identity_write`. This is deliberate.

`identity_write` is the scope that lets a credential read and write **another** agent's soul note
(`identity/*`). A freshly bootstrapped `main-agent`, or one recovered after a registry reset, does
not receive it, and so cannot read or write any other agent's identity. Owning your **own** soul
never requires it — an agent always reads and writes `identity/<its-own-subject>` — so nothing an
ordinary caller does is blocked by its absence.

**Why it is not granted by default.** Soul-write is a privilege of a different order from ordinary
writing: it overwrites the sovereign identity of another agent. It is therefore kept **disjoint**
from the ordinary write scopes (`write`, `admin`, `service`): a credential declared "full vault"
through `admin` does **not** silently inherit the power to rewrite another agent's soul. Handing it
out by default — to the bootstrap key, or as a side effect of `admin` — would erase precisely the
separation this release installs. Whichever credential provisions and repairs the souls of the
agents it supervises is meant to hold this as a distinct, narrowly-scoped grant, and nothing else.

**How to obtain it.** Include `identity_write` in the scope set of the credential that supervises
souls. Because it grants no *ordinary* write access on its own, it must be **combined with a write
scope** (`write`) — a key bearing `identity_write` alone is refused at creation as a key that
cannot write. For a dedicated supervisory identity:

```bash
gradatum-admin api-key create \
  --root <ROOT> \
  --owner <supervisor-identity> \
  --scopes identity_write,write
```

The owner must be declared in the ACL preset, exactly like the bootstrap identity — add a
`[[consumer]]` block if it is not.

To add the scope to an identity that **already** holds a key — for instance to let `main-agent`
itself manage other souls — you cannot use `rotate`: rotation carries the existing scope set over
unchanged. Since an identity holds only one active key at a time, revoke the current key and mint a
replacement with the expanded set:

```bash
gradatum-admin api-key revoke --root <ROOT> --prefix <PREFIX>
gradatum-admin api-key create \
  --root <ROOT> \
  --owner main-agent \
  --scopes vault_read,vault_search,vault_write,write,identity_write
```

Treat any credential carrying `identity_write` with the same care as an admin credential: grant it
narrowly, store it chmod-600, and rotate it on suspicion (see `SECURITY.md`).

---

## Attribution changes

Three behaviours change. Each one turns a silent fallback into an explicit outcome.

**The `X-Gradatum-Agent` header no longer selects an identity.** It is no longer read. Clients that
relied on it to distinguish themselves while sharing one key now all resolve to that key's owner.
To keep them distinct, give each client its own key — that is the supported mechanism, and it is
the only one that cannot be spoofed by the caller.

**An explicit `author` field on a write is refused.** Attribution is derived from the credential.
A client that sets it receives an error naming the field.

**An unresolvable caller is refused, not defaulted.** Requests that previously landed on a default
identity now fail. The refusal names what is missing and the command that fixes it.

Notes written before the upgrade are **not modified**. Their recorded author is historical data,
not a reference to the key store: revoking, rotating or deleting a key never alters a note that
already exists.

---

## One key per identity

From 2.0.0, an identity holds **one active key at a time**. `api-key create` refuses an identity
that already has one and points you to rotation:

```bash
gradatum-admin api-key rotate --root <ROOT> --prefix <PREFIX>
```

Rotation revokes the existing key and mints its replacement atomically, carrying the identity over
unchanged. If your deployment currently has several active keys for one identity, revoke the extra
ones before upgrading, or the invariant check will report them.

```bash
gradatum-admin api-key revoke --root <ROOT> --prefix <PREFIX>
```

---

## Pre-flight check

Copy-paste this before upgrading. It reports the two conditions that matter and exits non-zero if
either fails.

```bash
#!/usr/bin/env bash
set -uo pipefail
ROOT="${1:?usage: $0 <ROOT>}"
rc=0

echo "== active keys =="
out="$(gradatum-admin api-key list --root "$ROOT" 2>&1)" || { echo "$out"; exit 1; }
echo "$out"

# Only key rows are parsed: the listing also prints a header and a total line.
owners="$(grep -E '^ak_' <<<"$out" | awk '{print $2}')"

if ! grep -qx "main-agent" <<<"$owners"; then
  echo "FAIL: no key for identity 'main-agent' — the server will refuse every request after upgrade."
  rc=1
fi

dupes="$(sort <<<"$owners" | uniq -d | tr '\n' ' ')"
if [ -n "${dupes// /}" ]; then
  echo "FAIL: these identities hold more than one active key: $dupes"
  rc=1
fi

[ "$rc" -eq 0 ] && echo "OK: ready to upgrade."
exit "$rc"
```

---

## Rolling back

2.0.0 changes no schema and no stored data. Rolling back is redeploying the previous binaries.

Keys minted under 2.0.0 remain valid on 1.0.0 — the owner column they use predates this release.
The reverse is also true: nothing created before the upgrade is invalidated by it.

If you edited the ACL preset, restore your backup and restart, since an unreadable preset makes the
server refuse every locus.

---

## Resetting the key registry

2.0.0 adds a way to return an installation to a clean credential state, for redeployments and for
recovering from a lost key set. It revokes every key, leaving the audit trail intact, and returns
the server to the uninitialised state the bootstrap step starts from.

It touches the **key registry only**. Notes, their content and their attribution are never affected.

It locks out every client at once and requires explicit confirmation. Treat it as a maintenance
operation, not a routine one — after it, every identity needs its key re-issued.

The administration tool operates directly on the root directory rather than through the
authenticated API, so losing every key never removes your ability to mint a new one.

After a reset on a multi-tenant server, unauthenticated requests receive the same 503 as a
never-initialised registry until the bootstrap key is re-minted. This is an accepted, documented
disclosure — see "Registry-state disclosure" in `SECURITY.md`.

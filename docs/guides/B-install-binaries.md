# Guide B — Install from pre-built binaries

**Platform: Linux x86_64.** Recommended path for deploying gradatum without a Rust toolchain.
For arm64/macOS/Windows, or to run the test suite, see
[Guide C — Build from source](C-build-from-source.md).

---

## Prerequisites

**glibc 2.34 or newer** (x86_64). The archives below are dynamically linked against glibc; a host
older than 2.34 loads them with `version 'GLIBC_2.xx' not found` **at startup**, not a clean
error. 2.34 covers Debian 12 (2.36), Ubuntu 22.04 LTS (2.35), RHEL / Rocky / AlmaLinux 9 (2.34)
and anything newer. Check yours:

```
ldd --version | head -1     # e.g. "ldd (Ubuntu GLIBC 2.35-...) 2.35"
```

If your glibc is older, build from source ([Guide C](C-build-from-source.md)) or run the Docker
image ([Guide A](A-docker-quickstart.md)) — the container carries its own glibc. This floor is
the normative platform statement in
[docs/DEPLOYMENT.md § Platform support](../DEPLOYMENT.md#platform-support).

---

## Two release paths — read this first

Gradatum has **two separate CI release workflows**, and they do not ship the same thing:

| | `.forgejo/workflows/release.yml` | `.github/workflows/release.yml` |
|---|---|---|
| Runs on | self-hosted runners | GitHub-hosted (`ubuntu-latest`) |
| Publishes to | internal only, not publicly reachable | **GitHub Releases** (public) |
| Archive shape | **two** role-based archives (`gradatum-server-*`, `gradatum-llm-*`), same naming/layout as the GitHub path — 6 binaries are compiled and glibc-floor-verified (`server`, `worker`, `admin`, `cli`, `engine`, `gateway`), but `gradatum-cli` is not packaged into either archive | **two** role-based archives (`gradatum-server-*`, `gradatum-llm-*`) + a separate SBOM tarball — `gradatum-cli` is explicitly excluded from the build step |
| SBOM / SLSA attestation | none | yes — CycloneDX SBOM (publishable crates only) + `actions/attest-build-provenance` |

**The archives below, and this whole guide, describe the GitHub path** — that is what
`https://github.com/gradatum/gradatum/releases` serves, and what the README points to.

**The GitHub repository is a push mirror, not auto-synced.** A tag may exist upstream before it
is mirrored to GitHub — mirroring is a separate, manual step in this project's current release
process. Practical consequence: **a release may not yet be available on GitHub even though it
has been tagged.** `scripts/fetch-gradatum-release.sh` checks the GitHub Releases API for the
requested tag **before** downloading anything, and fails with an explicit message rather than a
silent 404 if the tag isn't there yet. If you hit that failure, retry later — the release is on
its way.

---

## Archives

Each [GitHub Release](https://github.com/gradatum/gradatum/releases) ships:

| Archive | Binaries inside | Deploy on |
|---|---|---|
| `gradatum-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-server`, `gradatum-worker`, `gradatum-admin` | **app-host** — vault backbone |
| `gradatum-llm-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gradatum-gateway`, `gradatum-engine` | **gpu-host** (engines) + **app-host** (gateway) |
| `gradatum-sbom-vX.Y.Z.tar.gz` | One CycloneDX SBOM (`.cdx.json`) per publishable crate | Supply-chain review |

A single `SHA256SUMS` covers all archives above. Each binary archive ships with an individual
SLSA provenance attestation (`actions/attest-build-provenance`, verifiable via the `gh` CLI).

**`gradatum-cli` is not part of any of these archives.** It is a stub (`main.rs` prints
`not yet implemented` and exits) — the GitHub release build step explicitly excludes it. The
crate was published once, at `0.7.6`, and remains installable from crates.io at that version
only; it is not republished at later tags. See [Guide C §crates.io](C-build-from-source.md) for
the exact caveat. (The Docker image also excludes it — see
[Guide A §Docker image](A-docker-quickstart.md): its `Dockerfile` names five `--bin` targets
and `gradatum-cli` is not one of them, so it is neither built nor shipped there either.)

**None of these archives contain systemd unit files, the `sysusers.d` entry, or example TOML
configs** — those live only in the git repository (`packaging/`, `examples/configs/`). Use
`scripts/fetch-gradatum-release.sh` (fetches binaries **and** those files from the matching
source tarball in one pass) or `git clone` the repository. See `packaging/systemd/README.md`
for the exact file list.

---

## Download, verify, install

**Automated (recommended):**

```bash
bash scripts/fetch-gradatum-release.sh --version v2.0.0 --group server --dest /usr/local/bin
```

`--group` selects which archive(s) to fetch (`server` | `llm` | `all`). Repeat with a
different `--group` for each role you deploy (e.g. `llm` on the GPU host). The script verifies
`SHA256SUMS`, verifies the SLSA attestation when `gh` is available, and by default also fetches
`packaging/` + `examples/configs/` from the source tarball of the same tag. `--dry-run` prints
the plan without downloading or writing anything.

**Manual (equivalent steps, server archive example):**

```bash
VERSION=v2.0.0
ARCH=x86_64-unknown-linux-gnu

curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/gradatum-server-${VERSION}-${ARCH}.tar.gz"
curl -fLO "https://github.com/gradatum/gradatum/releases/download/${VERSION}/SHA256SUMS"

sha256sum -c SHA256SUMS --ignore-missing

# Optional — requires gh CLI v2.49+
gh attestation verify "gradatum-server-${VERSION}-${ARCH}.tar.gz" --repo gradatum/gradatum

tar -xzf "gradatum-server-${VERSION}-${ARCH}.tar.gz"
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-server" /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-worker"  /usr/local/bin/
sudo install -m 755 "gradatum-server-${VERSION}-${ARCH}/gradatum-admin"   /usr/local/bin/
```

Repeat for `gradatum-llm` on the host running inference engines.

---

## systemd

The repo ships an idempotent install script that wires the binaries above into systemd units,
creates the `gradatum` system user, and initializes the vault:

```bash
sudo bash scripts/install-gradatum-services.sh --build
sudo bash scripts/install-gradatum-services.sh --build --with-engine
sudo bash scripts/install-gradatum-services.sh --build --with-engine --with-gateway
```

`--build` compiles from source (`cargo build --release`) as part of the install — if you're
installing from a fetched binary release instead, drop `--build` and place the binaries under
`target/release/` yourself first (the script reads from there, not from your `--dest`). For
subsequent deploys (binary swap without re-init):

```bash
bash scripts/deploy-gradatum-local.sh --build
bash scripts/deploy-gradatum-local.sh --build --engine
```

**`gradatum-worker` needs `GRADATUM_INTERNAL_TOKEN` — the install script sets it for you.**
`gradatum-worker.service` reads it from an environment file
(`EnvironmentFile=-/etc/gradatum/env`, `packaging/systemd/gradatum-worker.service`). Step
`[8/10]` of `install-gradatum-services.sh` runs after `gradatum-admin init` (which writes the
secret to `config/internal-worker.token.txt`, mode 0600, under `--root`, default
`/var/lib/gradatum`) and copies it into `/etc/gradatum/env` itself — owned `root:gradatum`,
mode 0640, readable by systemd (root) at unit start without being readable by the service
account's own session. The worker does not need this step run manually.

If `/etc/gradatum/env` is missing or lacks `GRADATUM_INTERNAL_TOKEN` for another reason (hand
rolled unit, file deleted after install), the worker fails immediately with
`Error: GRADATUM_INTERNAL_TOKEN must be set` and loops on that failure (`Restart=always`,
`RestartSec=15s`). Recreate it the same way the install script does:

```bash
echo "GRADATUM_INTERNAL_TOKEN=$(sudo cat /var/lib/gradatum/config/internal-worker.token.txt)" \
  | sudo tee /etc/gradatum/env
sudo chown root:gradatum /etc/gradatum/env
sudo chmod 0640 /etc/gradatum/env
sudo systemctl restart gradatum-worker
```

Full mechanism (why `init` writes the source token, its exact contents, why a self-generated
token does not work):
[Guide E — `server.toml` fields set by `gradatum-admin init`](E-ports-and-config.md#servertoml--fields-set-by-gradatum-admin-init).

Full unit reference: `packaging/systemd/README.md`. Multi-instance engine topology, sizing,
upgrade ordering (`gradatum-server` before `gradatum-worker`, and why), and troubleshooting:
[docs/DEPLOYMENT.md](../DEPLOYMENT.md).

---

## Ports and configuration

See [Guide E — Ports & configuration reference](E-ports-and-config.md) for the full port
matrix, override precedence, and the exact fields `gradatum-admin init` writes to
`server.toml`.

---

## Uninstall

There is no `--uninstall` flag on `install-gradatum-services.sh` — the steps below reverse what
it does, in the reverse order, using the same paths it writes to.

**Your vault lives entirely under `--root` (default `/var/lib/gradatum`)** — the SQLite DBs
(`db/queue.sqlite`, `db/revocation.sqlite`, `db/api_keys.sqlite`), the ACL preset
(`config/bearer.toml`), and the JWT signing key (`config/jwt-signing-key.secret` — losing it
invalidates every token issued so far). Back it up first if you want it back later:

```bash
sudo tar -czf ~/gradatum-vault-backup.tar.gz -C / var/lib/gradatum
```

**1. Stop and disable the services** (does not touch data):

```bash
sudo systemctl disable --now gradatum-worker.service gradatum-server.service
# If installed with --with-engine: each instance was enabled separately
# (systemctl enable --now gradatum-engine@<name>), so disable each by name —
# find the running ones first:
systemctl list-units 'gradatum-engine@*' --all
sudo systemctl disable --now gradatum-engine@<name>.service   # repeat per instance
# If installed with --with-gateway:
sudo systemctl disable --now gradatum-gateway-spike.service
```

**2. Remove the unit files and drop-ins** (does not touch data):

```bash
sudo rm -f /etc/systemd/system/gradatum-server.service \
           /etc/systemd/system/gradatum-worker.service \
           /etc/systemd/system/gradatum-engine@.service \
           /etc/systemd/system/gradatum-gateway-spike.service
sudo rm -rf /etc/systemd/system/gradatum-server.service.d \
            /etc/systemd/system/gradatum-worker.service.d \
            /etc/systemd/system/gradatum-engine@.service.d \
            /etc/systemd/system/gradatum-gateway-spike.service.d
sudo systemctl daemon-reload
```

**3. Remove the binaries** (does not touch data):

```bash
sudo rm -f /usr/bin/gradatum-server /usr/bin/gradatum-worker /usr/bin/gradatum-admin
sudo rm -f /usr/local/bin/gradatum-pre-migration-backup
sudo rm -rf /opt/gradatum/bin   # gradatum-engine / gradatum-gateway, if installed
```

**4. Remove the configuration** (does not touch the vault itself, but does remove the worker's
`GRADATUM_INTERNAL_TOKEN` file and any engine/gateway configs):

```bash
sudo rm -rf /etc/gradatum
```

**5. Remove the vault data — destructive, only after your backup from above is confirmed good**:

```bash
sudo rm -rf /var/lib/gradatum
sudo rm -rf /opt/gradatum/models   # only if you want the GGUF files gone too
```

**6. Remove the service account** — only if the install script created it for you (skip this if
you passed `--user` naming an account that pre-existed, since the script reused it as-is
without taking ownership of it):

```bash
sudo userdel gradatum
sudo rm -f /usr/lib/sysusers.d/gradatum.conf   # only present for the default 'gradatum' name
```

Steps 1–4 get you back to "gradatum was never installed here" while keeping every note in the
vault. Steps 5–6 are the only ones that discard data or the service identity — do them last, and
only once you're sure.

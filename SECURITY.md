# SECURITY.md

> Vulnerability disclosure policy for Gradatum.

---

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Send a report to `security@gradatum.org` (PGP key fingerprint published on `gradatum.org/security` once the public release ships; until then, reports may be sent in plain text and will be triaged with the same urgency).

Include:

- A clear description of the issue and its impact.
- Steps to reproduce (proof-of-concept code or commands).
- The Gradatum version and commit hash you tested against.
- Your name / handle for credit (optional — anonymous reports are accepted).

---

## Response timeline

| Phase | Target |
|---|---|
| Acknowledgement | Within **72 hours** of receipt. |
| Triage + severity assessment | Within **7 days**. |
| Fix or mitigation | Within **30 days** for High / Critical, **90 days** for Medium / Low. |
| Public disclosure | Coordinated with reporter; default 90 days after triage or 14 days after fix release, whichever comes first. |

---

## Severity classification

We use [CVSS 3.1](https://www.first.org/cvss/v3.1/specification-document) scoring. Severity bands:

| CVSS score | Band |
|---|---|
| 9.0–10.0 | Critical |
| 7.0–8.9 | High |
| 4.0–6.9 | Medium |
| 0.1–3.9 | Low |

---

## Supported versions

While Gradatum is in `0.x` (Alpha / Beta), only the latest tag receives security fixes.

After `1.0`:

| Version | Status | Security fixes |
|---|---|---|
| `1.x` LTS branch | Supported | Yes — for the LTS lifetime declared at `1.0` release. |
| `1.x` main | Supported | Yes — fixes ported from LTS. |
| `0.x` | End-of-life at `1.0` | No backports. |

The exact LTS lifetime (e.g. 18 or 24 months) is decided in the `1.0` release RFC.

---

## Hardening defaults

By design, Gradatum applies these defaults at boot time:

- **Bind loopback only** (`127.0.0.1`) unless explicitly overridden + TLS configured. The server refuses to boot on `0.0.0.0` without TLS (caveat C3, fail-closed).
- **JWT audience-scoped strict** (`aud=service-X` exact match), Ed25519, mandatory `kid` header, TTL 1 h (caveat C1, decision D6).
- **Persistent revocation store** required (caveat C2). In-memory store is allowed in dev with a `WARN` log on every boot.
- **Gateway body logging is opt-in** and encrypted at rest, with sanitised headers (caveat C11).
- **OpenDAL backends gated by feature flags** — only the backends explicitly enabled are compiled in (caveat C12).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full security design and caveats.

---

## Supply chain

- `cargo audit` and `cargo deny` run **daily** in CI with `fail-on-finding` (caveat C8).
- All dependencies are pinned with `=` for critical workspace deps (decision R11).
- A SBOM (CycloneDX) is published with every release.
- Vendored headers are included in the release tarball; checksums published.

---

## Acknowledgements

A `SECURITY-HALL-OF-FAME.md` will be published once the first vulnerability is resolved. Reporters are credited with their preferred handle (or kept anonymous on request).

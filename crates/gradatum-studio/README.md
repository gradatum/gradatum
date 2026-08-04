# gradatum-studio

> Web-based operator UI for Gradatum — React + TypeScript, served by `gradatum-server` at `/ui/*`.

**Status**: v1.0.0 — public, `Apache-2.0 AND OFL-1.1 AND MIT AND ISC`. Bundle-only crate: no
public Rust API. The crate's own code is Apache-2.0; the shipped `dist/` bundle embeds font
assets under OFL-1.1 and npm dependencies under MIT and ISC — full notices in
[`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md).
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-studio` is the admin Studio shipped as a React + TypeScript + Vite bundle,
built in CI, committed to the repository, and served by `gradatum-server` via `tower-http`
`ServeDir` at `/ui/*`.
A SPA fallback (`index.html`) handles client-side routing; deep-links and browser refreshes
work correctly.

Key properties:

- **Authentication** — login screen accepts an API key (`ak_…`); the key is exchanged for a
  JWT (`POST /auth/exchange`) and stored in `localStorage` (`gradatum_studio_jwt_persist`,
  1 h TTL — the server default `jwt_ttl_human_secs = 3600` — with client-side expiry check). The original API key is never persisted after
  exchange. **Any valid, non-revoked key is accepted**: `/auth/exchange` checks the Argon2id
  hash and the tenant allow-list, never the key's scopes — the `scope: "human"` the client
  sends only selects the TTL. Scopes are enforced on write paths alone, and only under
  `multi_tenant.enabled = true` (`write`, `admin` or `service`); `admin` is therefore a
  recommended convention, not a login requirement.
- **Security headers** — strict CSP, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `Permissions-Policy` on all static assets.
- **No telemetry** — the Studio never contacts external services.

## Pages

| Page | Route | Description |
|---|---|---|
| `LoginPage` | `/login` | API key entry + JWT exchange |
| `DashboardPage` | `/` | Vault summary, job queue snapshot, review count badge, scheduler health widget |
| `NotesPage` | `/notes` | Paginated note listing with status and section filters |
| `NoteDetailPage` | `/notes/:id` | Note body, metadata, re-curate action |
| `SearchPage` | `/search` | Vault search (BM25 + semantic) with snippet display |
| `ReviewPage` | `/review` | Pending-review queue — promote or downgrade notes |
| `JobsPage` | `/jobs` | Apalis job queue — filter by status/kind, manual trigger |
| `SystemPage` | `/system` | Scheduled task health (badges: ok / error / overdue) + Metrics section (uPlot time-series charts, 1h/24h/7d/14d range selector, 60s auto-refresh) |
| `ActivityPage` | `/activity` | Agent trace log — filterable by action type and time range, expandable rows, 60s auto-refresh |

## Scheduled task health

`SystemPage` polls `GET /api/v1/system/scheduled` and renders a per-task status row:

- **ok** — last run within expected interval, no errors.
- **error** — last run returned an error (`last_outcome = "error"`).
- **overdue** — `now - last_run_ms > interval_secs × margin` (computed client-side from
  `interval_secs` returned by the endpoint).
- `errors_24h` — count of errors in the last 24 hours; highlighted in red when > 0.

`DashboardPage` shows a compact scheduler widget (task count, how many in error or overdue)
linking to `SystemPage`.

## Curated metrics charts

The Metrics section in `SystemPage` renders interactive uPlot time-series charts, consuming
`GET /api/v1/system/metrics/catalog` and `GET /api/v1/system/metrics/timeseries`.

- **Range selector** — `1h` / `24h` (default) / `7d` / `14d`.
- **Groups** — four collapsible sections: `usage` / `context` / `server` / `write`.
- **Counter series** — displayed as rate-per-minute (client-side delta derivation).
- **Histogram series** — displayed as per-interval average (`_sum` / `_count`).
- **Gauge series** — raw values.
- **Uninstrumented metrics** — shown greyed out rather than hidden.
- **Auto-refresh** — every 60 seconds (toggle on/off, default on).

Dependency: `uplot@^1.6.32` (MIT, ~15 KB gzip).

## Build

The bundle is produced by the `studio-build` CI job (`npm ci` → `npm audit` → `npm run build`
→ `npm test`), which runs Vite inside this crate's directory. There is **no** `build.rs`
integration: no Cargo build script invokes npm, and `gradatum-server` does not depend on this
crate — the two are linked only at runtime, by a filesystem path.

The resulting `dist/` bundle is committed to the repository and shipped inside the published
`.crate`, so a fresh checkout carries a ready-to-serve bundle. At runtime `gradatum-server`
serves it with `tower-http`'s `ServeDir`, from the directory given by the `[studio] ui_dir`
configuration key.

```bash
# Build the studio manually (from the crate directory)
npm ci
npm run build
```

Note: the bundle is a build artifact under version control. After changing anything under
`src/`, rebuild and commit the regenerated `dist/`, otherwise the shipped bundle no longer
matches the sources it claims to be built from.

## License

The crate's own code (Rust and TypeScript sources) is licensed under **Apache-2.0**.

The distributed `dist/` bundle additionally incorporates third-party material:

- **Fonts** — IBM Plex Sans, JetBrains Mono, Spectral, under **OFL-1.1**.
- **npm dependencies** — 68 packages under **MIT**, 1 package under **ISC**.

Full copyright notices and license texts, reproduced verbatim from the packages actually
embedded in the bundle: [`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md).

SPDX: `Apache-2.0 AND OFL-1.1 AND MIT AND ISC`

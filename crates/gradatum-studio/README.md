# gradatum-studio

> Web-based operator UI for Gradatum — React + TypeScript, served by `gradatum-server` at `/ui/*`.

**Status**: v0.7.6 — `publish = false` (not published to crates.io). Internal UI crate.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-studio` is the admin Studio shipped as a React + TypeScript + Vite bundle,
built at compile time and served by `gradatum-server` via `tower-http` `ServeDir` at `/ui/*`.
A SPA fallback (`index.html`) handles client-side routing; deep-links and browser refreshes
work correctly.

Key properties:

- **Authentication** — login screen accepts an API key (`ak_…`); the key is exchanged for a
  JWT (`POST /auth/exchange`) and stored in `localStorage` (`gradatum_studio_jwt_persist`,
  24 h TTL with client-side expiry check). The original API key is never persisted after
  exchange. Requires `admin` scope.
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

The Studio is built as part of the workspace via a `build.rs` script in `gradatum-server`
that runs `npm run build` inside this crate's directory. The resulting `dist/` bundle is
served at runtime from the directory configured by `[studio] ui_dir`. A pre-built `dist/`
is checked in for convenience.

```bash
# Build the studio manually (from the crate directory)
npm install
npm run build
```

## License

Apache-2.0

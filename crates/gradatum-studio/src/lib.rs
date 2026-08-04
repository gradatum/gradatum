//! gradatum-studio — a "bundle-only" crate.
//!
//! This crate contains no functional Rust code and exposes no public API.
//! It hosts the npm project (React + TypeScript + Vite) whose build produces
//! the `dist/` directory, the only real deliverable of this package.
//!
//! # Build
//!
//! The bundle is produced by the `studio-build` continuous integration job
//! (`npm ci` → `npm audit` → `npm run build` → `npm test`). No Cargo build
//! script invokes npm: compiling this crate has no effect on the bundle.
//! The `dist/` directory is checked into the repository and embedded as is
//! into the published package.
//!
//! # Asset serving
//!
//! `gradatum-server` serves the contents of `dist/` under `/ui/*` through the
//! `ServeDir` of `tower-http`, from the directory designated by the
//! `[studio] ui_dir` configuration key. No Cargo dependency links the two
//! crates: the coupling is a filesystem path, resolved at runtime.
//!
//! The bundle is served without authentication — it is a public static
//! artifact. Authentication is carried by the API calls issued from the
//! browser, which present a JWT as `Bearer`.
//!
//! # Licensing
//!
//! The code of this crate is under Apache-2.0. The distributed bundle
//! incorporates fonts under OFL-1.1 and npm dependencies under MIT and ISC;
//! the full notices live in `THIRD-PARTY-LICENSES.md`, at the root of the
//! package.

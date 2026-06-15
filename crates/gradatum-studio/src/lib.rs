//! gradatum-studio — bundle-only crate.
//!
//! Ce crate ne contient pas de code Rust fonctionnel. Il héberge le projet npm
//! (React + TypeScript + Vite) dont le build produit `dist/` — servi par
//! gradatum-server via tower-http `ServeDir` sur `/ui/*`.
//!
//! # Build
//!
//! Le build npm est déclenché par le job CI `studio-build` (runner web dédié,
//! sans label de runner CI interne). Le binaire gradatum-server s'attend à trouver
//! les assets dans le répertoire configuré par `[studio] ui_dir`.
//!
//! # Déploiement
//!
//! Les assets compilés sont copiés dans `/usr/share/gradatum/ui/` par le
//! script de déploiement. Le serveur sert ces fichiers sans authentification
//! (le bundle est public ; toutes les API calls portent le Bearer JWT).

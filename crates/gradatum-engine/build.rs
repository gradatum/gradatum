//! Script de build — injecte le SHA git court au compile-time.
//!
//! Émet `BUILD_SHA=<sha>` via `cargo:rustc-env` pour que le binaire expose
//! un identifiant traçable dans sa sortie `--version`.
//!
//! # ECON: copie conforme de `crates/gradatum-server/build.rs`
//!
//! Un build script n'est PAS héritable : `cargo:rustc-env` ne vaut que pour le
//! crate qui porte le script. Le SHA doit donc être capturé par chaque binaire.
//! Duplication assumée à **4 occurrences** (server, worker, gateway, engine) —
//! le seuil de la règle des trois est franchi.
//!
//! Upgrade → extraction différée dans un crate `gradatum-buildinfo` (feuille,
//! sans dépendance) exposant `pub const BUILD_SHA`. Décision : reportée — créer
//! ce crate ferait passer la surface publiée de 27 à 28 crates au moment d'un
//! tag ; la dette est tracée ailleurs. Ne PAS placer ce script dans un crate lib
//! déjà largement dépendu (`gradatum-core`) : le `rerun-if-changed` sur `HEAD` y
//! déclencherait la recompilation en cascade des 29 crates dépendantes à chaque
//! commit.
//!
//! Contrôle de dérive : le corps de ce fichier doit rester identique à celui du
//! server — `diff <(tail -n +N crates/gradatum-engine/build.rs) <(tail -n +M crates/gradatum-server/build.rs)`.
//!
//! # Robustesse
//!
//! Si `git` est absent, le dépôt inaccessible, ou la commande échoue pour
//! quelque raison que ce soit → fallback silencieux à `"unknown"`.
//! Le build ne doit JAMAIS échouer à cause de ce script.
//!
//! # Worktree-safety (DT-OBS-2)
//!
//! En `git worktree`, le fichier `HEAD` et l'index se trouvent dans un répertoire
//! `.git/worktrees/<nom>/` distinct du `.git/` racine. Les chemins relatifs hardcodés
//! (`../../.git/HEAD`) peuvent être incorrects et ne déclencher aucun re-build.
//!
//! Stratégie :
//! 1. `git rev-parse --git-path HEAD` → chemin absolu du HEAD de la worktree courante.
//! 2. `git rev-parse --git-path index` → chemin absolu de l'index de la worktree courante.
//! 3. Fallback vers les chemins relatifs historiques si la commande échoue.
//!
//! `cargo:rerun-if-changed` accepte les chemins inexistants sans erreur.

fn main() {
    // DT-OBS-2 — rerun-if-changed worktree-safe.
    //
    // `git rev-parse --git-path HEAD` retourne le chemin ABSOLU du fichier HEAD
    // pour la worktree courante (ex. `.git/worktrees/agent-xxx/HEAD` en worktree,
    // `.git/HEAD` en checkout classique). Cela garantit que le re-build est
    // déclenché par le BON fichier HEAD, quelle que soit la topologie git.
    let head_path = resolve_git_path("HEAD").unwrap_or_else(|| "../../.git/HEAD".to_owned());
    let index_path = resolve_git_path("index").unwrap_or_else(|| "../../.git/index".to_owned());

    println!("cargo:rerun-if-changed={head_path}");
    println!("cargo:rerun-if-changed={index_path}");

    let sha = resolve_git_sha();
    println!("cargo:rustc-env=BUILD_SHA={sha}");
}

/// Résout le chemin absolu d'un fichier git interne via `git rev-parse --git-path <name>`.
///
/// Retourne `None` si `git` est absent, si la commande échoue, ou si la sortie est vide.
/// En worktree, le chemin pointe vers le répertoire `.git/worktrees/<nom>/` plutôt
/// que `.git/` racine — ce qui est le comportement attendu.
fn resolve_git_path(name: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Tente de résoudre le SHA court du commit HEAD via `git rev-parse --short HEAD`.
///
/// Retourne `"unknown"` si :
/// - `git` n'est pas dans le `PATH`,
/// - la commande retourne un code non-zéro (pas un repo, HEAD détaché sans commit, etc.),
/// - la sortie contient des caractères invalides (UTF-8 inattendu),
/// - la sortie est vide après trim.
fn resolve_git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

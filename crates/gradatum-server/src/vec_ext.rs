//! Enregistrement de l'extension sqlite-vec (`vec0`) pour `gradatum-server`.
//!
//! ## Pourquoi dans ce module
//!
//! `gradatum-index` a `#![forbid(unsafe_code)]` — aucun `unsafe` n'est permis dans
//! la bibliothèque d'index. L'enregistrement de l'extension sqlite-vec nécessite
//! un appel `unsafe` (`sqlite3_auto_extension`) qui doit donc vivre dans un bin crate.
//!
//! Ce module est déclaré dans `main.rs` du bin `gradatum-server` et reste hors du
//! chemin de test unitaire. L'appel doit être effectué une seule fois, AVANT toute
//! ouverture de connexion SQLite via `SqliteIndex::open`.
//!
//! ## Sécurité
//!
//! `sqlite3_auto_extension` est une API C qui enregistre une fonction d'initialisation
//! appelée automatiquement à l'ouverture de chaque connexion SQLite. Elle est conçue
//! pour être appelée avant toute connexion ouverte et est idempotente (plusieurs appels
//! avec la même fn = no-op côté SQLite). L'ABI du pointeur de fonction est garantie
//! par la crate `sqlite-vec 0.1.9` qui expose `sqlite3_vec_init` avec la signature
//! attendue par `sqlite3_auto_extension`.
//!
//! ## Feature gate
//!
//! Cette fonction n'est disponible que si la feature `sqlite-vec-ext` du crate
//! `gradatum-server` est active. Dans les tests qui n'ont pas chargé l'extension,
//! `SqliteIndex::ann_is_enabled()` reste `false` et le chemin brute-force s'applique.

// Ce module contient du code `unsafe` : il enregistre l'extension sqlite-vec C.
// Toute modification doit faire l'objet d'une revue de la justification SAFETY ci-dessous.
#![allow(unsafe_code)]

/// Enregistre l'extension sqlite-vec (`vec0`) pour toutes les connexions SQLite futures.
///
/// Doit être appelé UNE SEULE FOIS, avant toute ouverture de connexion SQLite via
/// `SqliteIndex::open` ou `SqliteIndex::open_in_memory`.
///
/// ## Idempotence
///
/// SQLite déduplique les appels `sqlite3_auto_extension` à la même adresse de fonction :
/// appeler cette fonction plusieurs fois est sans effet supplémentaire.
///
/// ## Erreur
///
/// Retourne `Err` si `sqlite3_auto_extension` renvoie un code d'erreur différent de
/// `SQLITE_OK`. En pratique, ce cas ne se produit pas avec une version correcte de
/// libsqlite3. En cas d'erreur, le serveur doit refuser de démarrer en mode SqliteVec.
///
/// # Panics
///
/// Ne panic jamais. Toutes les erreurs sont propagées via `Result`.
pub fn register_sqlite_vec() -> Result<(), String> {
    // SAFETY: `sqlite3_vec_init` respecte la signature de `sqlite3_auto_extension` :
    // `extern "C" fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int`.
    // sqlite-vec 0.1.9 expose la fonction avec la déclaration conservative C `extern "C" fn()`
    // (sans paramètres, idiome C courant pour les pointeurs de fn à signature variable).
    // Le transmute vers le type `Option<unsafe extern "C" fn()>` attendu par rusqlite
    // est safe dans ce contexte : l'ABI C est identique (calling convention identique
    // x86-64 System V). Le même transmute est utilisé dans les tests de sqlite-vec 0.1.9
    // (voir `sqlite-vec/src/lib.rs`).
    // Cette fonction est conçue pour être appelée avant toute ouverture de connexion
    // (précondition documentée par SQLite : `sqlite3_auto_extension` doit être appelé
    // avant `sqlite3_open`). La sûreté des connexions ouvertes APRÈS cet appel est
    // garantie par l'extension elle-même (pas de mutation globale non thread-safe).
    #[expect(
        clippy::missing_transmute_annotations,
        reason = "type cible dépend de l'ABI rusqlite/libsqlite3-sys interne — \
                  annoter introduirait une dépendance fragile sur les types privés rusqlite"
    )]
    let rc = unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )))
    };

    if rc == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(format!(
            "sqlite3_auto_extension(vec0) a retourné le code d'erreur {rc} — \
             extension sqlite-vec non chargée"
        ))
    }
}

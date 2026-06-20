//! Tests golden pour `parse_bash_file` (feature `code-bash`).
//!
//! ## Principe
//!
//! Ces tests utilisent des snippets Bash représentatifs pour vérifier :
//! 1. Les fonctions extraites (`function_definition`) → `kind = "fn"`.
//! 2. Les deux formes syntaxiques (`foo(){}` et `function foo {}`).
//! 3. Les commentaires précédant une fonction → `doc_comment`.
//! 4. Les assignments top-level → `kind = "const"`.
//! 5. Le propagation de `source_path`.
//! 6. L'idempotence : même contenu parsé deux fois → mêmes symboles.
//! 7. Source vide → zéro symboles.
//!
//! ## Décisions de design (accuracy > coverage)
//!
//! - Visibilité : toujours `"pub"` (Bash n'a pas de modificateur syntaxique).
//! - Signature : toujours `None` (pas de paramètres typés en Bash).
//! - Deps : toujours `vec![]` pour cette implémentation (accuracy > coverage).

#![cfg(feature = "code-bash")]

use gradatum_ingest::parse_bash_file;

// ── Snippet Bash réaliste ─────────────────────────────────────────────────────

const SNIPPET_GOLDEN: &str = r#"#!/usr/bin/env bash
# Script de déploiement gradatum.

CONFIG_FILE=/etc/gradatum/config.toml
LOG_LEVEL=info

# Vérifie que les prérequis sont installés.
# Retourne 1 si un outil manque.
check_prerequisites() {
    command -v cargo >/dev/null 2>&1 || return 1
    command -v git >/dev/null 2>&1 || return 1
}

# Compile le binaire en mode release.
function build_release {
    cargo build --release --bin gradatum-server
}

# Déploie le binaire sur le LXC cible.
deploy() {
    local target=$1
    scp target/release/gradatum-server "$target":/opt/gradatum/
}

_internal_helper() {
    echo "internal"
}
"#;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn find_sym<'a>(
    symbols: &'a [gradatum_ingest::DerivedSymbol],
    name: &str,
) -> Option<&'a gradatum_ingest::DerivedSymbol> {
    symbols.iter().find(|s| s.qualified_name == name)
}

// ── Test ABI : abi_bash_check.rs couvre déjà set_language + parse triviale.

// ── Golden test 1 : fonction style `foo()` ───────────────────────────────────

/// Vérifie que `check_prerequisites` (forme `foo(){}`) est extrait comme `fn`.
#[test]
fn golden_extracts_check_prerequisites_as_fn() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let sym =
        find_sym(&symbols, "check_prerequisites").expect("check_prerequisites doit être extrait");
    assert_eq!(sym.kind, "fn", "check_prerequisites kind = fn");
    assert_eq!(sym.visibility, "pub", "Bash = tout pub");
    assert_eq!(sym.source_path, "scripts/deploy.sh", "source_path propagé");
    // Signature = None (Bash ne déclare pas de paramètres typés)
    assert!(
        sym.signature.is_none(),
        "signature doit être None pour Bash (pas de déclaration de paramètres)"
    );
}

// ── Golden test 2 : forme `function foo {}` ───────────────────────────────────

/// Vérifie que `build_release` (forme `function foo {}`) est extrait comme `fn`.
#[test]
fn golden_extracts_build_release_as_fn() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let sym = find_sym(&symbols, "build_release").expect("build_release doit être extrait");
    assert_eq!(sym.kind, "fn", "build_release kind = fn");
    assert_eq!(sym.visibility, "pub", "Bash = tout pub");
}

// ── Golden test 3 : fonction `deploy` ────────────────────────────────────────

/// Vérifie que `deploy` est extrait comme `fn`.
#[test]
fn golden_extracts_deploy_as_fn() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let sym = find_sym(&symbols, "deploy").expect("deploy doit être extrait");
    assert_eq!(sym.kind, "fn");
    assert_eq!(sym.visibility, "pub");
}

// ── Golden test 4 : fonction interne extraite (Bash n'a pas de privé) ────────

/// Vérifie que `_internal_helper` est extrait (pas de filtre privé en Bash).
#[test]
fn golden_extracts_internal_helper() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let sym = find_sym(&symbols, "_internal_helper")
        .expect("_internal_helper doit être extrait — Bash n'a pas de visibilité privée");
    assert_eq!(sym.kind, "fn");
    // En Bash, même les fonctions _préfixées sont "pub" (pas de modificateur syntaxique).
    assert_eq!(
        sym.visibility, "pub",
        "_internal_helper doit être pub — Bash n'a pas de modificateur de visibilité"
    );
}

// ── Golden test 5 : assignments top-level ────────────────────────────────────

/// Vérifie que les assignments top-level sont extraits comme `const`.
#[test]
fn golden_extracts_top_level_assignments_as_const() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let config =
        find_sym(&symbols, "CONFIG_FILE").expect("CONFIG_FILE doit être extrait comme const");
    assert_eq!(config.kind, "const", "CONFIG_FILE kind = const");
    assert_eq!(config.visibility, "pub");

    let log_level =
        find_sym(&symbols, "LOG_LEVEL").expect("LOG_LEVEL doit être extrait comme const");
    assert_eq!(log_level.kind, "const", "LOG_LEVEL kind = const");
}

// ── Golden test 6 : doc-comment extrait ──────────────────────────────────────

/// Vérifie que les commentaires précédant une fonction sont extraits comme doc.
#[test]
fn golden_extracts_doc_comment_from_preceding_comments() {
    let symbols = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("parse doit réussir");

    let sym =
        find_sym(&symbols, "check_prerequisites").expect("check_prerequisites doit être extrait");
    assert!(
        sym.doc_comment.is_some(),
        "check_prerequisites doit avoir un doc-comment (commentaires précédents)"
    );
    let doc = sym.doc_comment.as_ref().unwrap();
    assert!(
        doc.contains("Vérifie") || doc.contains("prérequis") || doc.contains("installés"),
        "doc-comment doit contenir le texte du commentaire, obtenu {:?}",
        doc
    );
}

// ── Golden test 7 : idempotence ──────────────────────────────────────────────

/// Même source parsée deux fois → mêmes noms de symboles dans le même ordre.
#[test]
fn golden_idempotent_parse() {
    let symbols_1 = parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("premier parse ok");
    let symbols_2 =
        parse_bash_file("scripts/deploy.sh", SNIPPET_GOLDEN).expect("deuxième parse ok");

    let names_1: Vec<&str> = symbols_1
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();
    let names_2: Vec<&str> = symbols_2
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();

    assert_eq!(
        names_1, names_2,
        "deux parses du même source doivent produire les mêmes symboles dans le même ordre"
    );
}

// ── Golden test 8 : source_path propagé ──────────────────────────────────────

/// Vérifie que `source_path` est correctement propagé dans tous les symboles.
#[test]
fn golden_source_path_propagated() {
    let path = "ops/deploy.sh";
    let symbols = parse_bash_file(path, SNIPPET_GOLDEN).expect("parse ok");

    for sym in &symbols {
        assert_eq!(
            sym.source_path, path,
            "source_path incorrect pour '{}'",
            sym.qualified_name
        );
    }
}

// ── Golden test 9 : source vide ──────────────────────────────────────────────

/// Vérifie que le parser gère une source vide sans panic.
#[test]
fn golden_empty_source() {
    let symbols = parse_bash_file("empty.sh", "").expect("parse ok");
    assert!(symbols.is_empty(), "source vide = zéro symboles");
}

// ── Golden test 10 : source minimale (commentaire seul) ──────────────────────

/// Vérifie que le parser gère correctement une source sans fonction ni assignment.
#[test]
fn golden_minimal_source_no_symbols() {
    let source = "# juste un commentaire\necho hello\n";
    let symbols = parse_bash_file("test.sh", source).expect("parse ok");
    // Un commentaire et une commande ne produisent pas de symboles extractibles.
    let fns: Vec<_> = symbols.iter().filter(|s| s.kind == "fn").collect();
    assert!(
        fns.is_empty(),
        "commentaire + echo ne produisent aucune fonction, got: {:?}",
        fns.iter().map(|s| &s.qualified_name).collect::<Vec<_>>()
    );
}

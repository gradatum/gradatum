//! Tests golden pour `parse_typescript_file` (feature `code-typescript`).
//!
//! ## Principe
//!
//! Ces tests utilisent un snippet TypeScript représentatif pour vérifier :
//! 1. Les fonctions exportées → `kind = "fn"`, `visibility = "pub"`.
//! 2. Les classes exportées → `kind = "class"`, `visibility = "pub"`.
//! 3. Les méthodes dans les classes → `kind = "method"`, `qualified_name = "Classe::method"`.
//! 4. Les interfaces exportées → `kind = "type"`, `visibility = "pub"`.
//! 5. Les arrow functions non-exportées → `kind = "fn"`, `visibility = "priv"`.
//! 6. Les docstrings JSDoc extraites.
//! 7. Le filtre `include_private`.
//! 8. L'idempotence.
//! 9. La propagation de `source_path`.
//! 10. Source vide → zéro symboles.
//!
//! ## Snippet de référence
//!
//! Le snippet correspond au cas d'usage fourni dans la spec F-61 inc3.

#![cfg(feature = "code-typescript")]

use gradatum_ingest::parse_typescript_file;

// ── Snippet TypeScript réaliste ───────────────────────────────────────────────

const SNIPPET_GOLDEN: &str = r#"/** Module de routage API. */

import { Request } from 'express';

/** Handler principale d'une requête. */
export function handleRequest(req: Request, body: string): Promise<void> {
    return fetch(body);
}

export class ApiRouter {
    /** Nom du router. */
    private name: string;

    constructor(name: string) {
        this.name = name;
    }

    /** Route une requête vers le handler. */
    public route(path: string): void {
        handleRequest({} as Request, path);
    }
}

export interface RouteConfig {
    path: string;
    method: string;
}

const internalHelper = (x: number) => x * 2;
"#;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn find_sym<'a>(
    symbols: &'a [gradatum_ingest::DerivedSymbol],
    name: &str,
) -> Option<&'a gradatum_ingest::DerivedSymbol> {
    symbols.iter().find(|s| s.qualified_name == name)
}

// ── Test ABI : abi_typescript_check.rs couvre déjà set_language + parse triviale.

// ── Golden test 1 : handleRequest → fn, pub ───────────────────────────────────

/// Vérifie que `handleRequest` est extrait comme `fn` exportée avec signature.
#[test]
fn golden_extracts_handle_request_as_fn() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "handleRequest").expect("handleRequest doit être extrait");
    assert_eq!(sym.kind, "fn", "handleRequest kind = fn");
    assert_eq!(sym.visibility, "pub", "handleRequest est exportée = pub");
    assert_eq!(sym.source_path, "src/router.ts", "source_path propagé");

    // Signature : doit contenir les paramètres
    assert!(
        sym.signature.is_some(),
        "handleRequest doit avoir une signature"
    );
    let sig = sym.signature.as_ref().unwrap();
    assert!(
        sig.contains("req") || sig.contains("body"),
        "signature doit contenir 'req' ou 'body', obtenu {:?}",
        sig
    );
}

// ── Golden test 2 : ApiRouter → class, pub ───────────────────────────────────

/// Vérifie que `ApiRouter` est extrait comme `class` exportée.
#[test]
fn golden_extracts_api_router_as_class() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "ApiRouter").expect("ApiRouter doit être extrait");
    assert_eq!(sym.kind, "class", "ApiRouter kind = class");
    assert_eq!(sym.visibility, "pub", "ApiRouter est exportée = pub");
    // Les classes n'ont pas de signature.
    assert!(
        sym.signature.is_none(),
        "ApiRouter ne doit pas avoir de signature"
    );
}

// ── Golden test 3 : ApiRouter::constructor → method, pub ─────────────────────

/// Vérifie que `ApiRouter::constructor` est extrait comme méthode publique.
#[test]
fn golden_extracts_constructor_as_method() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "ApiRouter::constructor")
        .expect("ApiRouter::constructor doit être extrait");
    assert_eq!(sym.kind, "method", "constructor kind = method");
    assert_eq!(
        sym.visibility, "pub",
        "constructor est pub par défaut (TS default)"
    );
}

// ── Golden test 4 : ApiRouter::route → method, pub (modificateur `public`) ───

/// Vérifie que `ApiRouter::route` est extrait comme méthode publique (modificateur `public`).
#[test]
fn golden_extracts_route_as_public_method() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "ApiRouter::route").expect("ApiRouter::route doit être extrait");
    assert_eq!(sym.kind, "method", "route kind = method");
    assert_eq!(
        sym.visibility, "pub",
        "route a le modificateur 'public' = pub"
    );
    // Signature : doit contenir le paramètre `path`
    assert!(sym.signature.is_some(), "route doit avoir une signature");
    let sig = sym.signature.as_ref().unwrap();
    assert!(
        sig.contains("path"),
        "signature doit contenir 'path', obtenu {:?}",
        sig
    );
}

// ── Golden test 5 : RouteConfig → type, pub ──────────────────────────────────

/// Vérifie que `RouteConfig` est extrait comme `type` (interface).
#[test]
fn golden_extracts_route_config_as_type() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "RouteConfig").expect("RouteConfig doit être extrait");
    assert_eq!(sym.kind, "type", "RouteConfig (interface) kind = type");
    assert_eq!(sym.visibility, "pub", "RouteConfig est exportée = pub");
}

// ── Golden test 6 : internalHelper → fn, priv ────────────────────────────────

/// Vérifie que `internalHelper` (const arrow non exportée) est extrait comme priv.
#[test]
fn golden_extracts_internal_helper_as_priv() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "internalHelper").expect("internalHelper doit être extrait");
    assert_eq!(sym.kind, "fn", "internalHelper (arrow) kind = fn");
    assert_eq!(
        sym.visibility, "priv",
        "internalHelper n'est pas exportée = priv"
    );
}

// ── Golden test 7 : filtre include_private=false ──────────────────────────────

/// Vérifie que les items non-exportés sont exclus quand `include_private=false`.
#[test]
fn golden_visibility_filter_excludes_private() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, false).expect("parse doit réussir");

    // internalHelper ne doit pas apparaître
    let internal = find_sym(&symbols, "internalHelper");
    assert!(
        internal.is_none(),
        "internalHelper ne doit pas être extrait quand include_private=false"
    );

    // Les items exportés doivent toujours être présents
    assert!(
        find_sym(&symbols, "handleRequest").is_some(),
        "handleRequest doit être extrait même avec include_private=false"
    );
    assert!(
        find_sym(&symbols, "ApiRouter").is_some(),
        "ApiRouter doit être extrait même avec include_private=false"
    );
    assert!(
        find_sym(&symbols, "RouteConfig").is_some(),
        "RouteConfig doit être extrait même avec include_private=false"
    );
}

// ── Golden test 8 : docstring JSDoc extraite ──────────────────────────────────

/// Vérifie que les docstrings JSDoc sont extraites.
#[test]
fn golden_jsdoc_extracted_for_handle_request() {
    let symbols =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let sym = find_sym(&symbols, "handleRequest").expect("handleRequest doit être extrait");
    assert!(
        sym.doc_comment.is_some(),
        "handleRequest doit avoir un doc-comment JSDoc"
    );
    let doc = sym.doc_comment.as_ref().unwrap();
    assert!(
        doc.contains("Handler") || doc.contains("requête"),
        "doc-comment doit contenir le texte JSDoc, obtenu {:?}",
        doc
    );
}

// ── Golden test 9 : idempotence ───────────────────────────────────────────────

/// Même source parsée deux fois → mêmes noms de symboles dans le même ordre.
#[test]
fn golden_idempotent_parse() {
    let symbols_1 =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("premier parse ok");
    let symbols_2 =
        parse_typescript_file("src/router.ts", SNIPPET_GOLDEN, true).expect("deuxième parse ok");

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

// ── Golden test 10 : source_path propagé ──────────────────────────────────────

/// Vérifie que `source_path` est correctement propagé dans tous les symboles.
#[test]
fn golden_source_path_propagated() {
    let path = "myproject/api/router.ts";
    let symbols = parse_typescript_file(path, SNIPPET_GOLDEN, true).expect("parse ok");

    for sym in &symbols {
        assert_eq!(
            sym.source_path, path,
            "source_path incorrect pour '{}'",
            sym.qualified_name
        );
    }
}

// ── Golden test 11 : source vide ──────────────────────────────────────────────

/// Vérifie que le parser gère une source vide sans panic.
#[test]
fn golden_empty_source() {
    let symbols = parse_typescript_file("empty.ts", "", true).expect("parse ok");
    assert!(symbols.is_empty(), "source vide = zéro symboles");
}

// ── Golden test 12 : ABI variante LANGUAGE_TYPESCRIPT vs TSX ──────────────────

/// Vérifie que le parser TypeScript parse correctement du TS simple (sans JSX).
#[test]
fn golden_basic_ts_without_jsx() {
    let source = "export function add(a: number, b: number): number { return a + b; }\n";
    let symbols = parse_typescript_file("utils.ts", source, true).expect("parse ok");

    assert_eq!(symbols.len(), 1, "un seul symbole attendu");
    let sym = &symbols[0];
    assert_eq!(sym.kind, "fn");
    assert_eq!(sym.qualified_name, "add");
    assert_eq!(sym.visibility, "pub");
    assert!(sym.signature.is_some());
    let sig = sym.signature.as_ref().unwrap();
    assert!(
        sig.contains("a") && sig.contains("b"),
        "signature doit contenir les params, obtenu {:?}",
        sig
    );
}

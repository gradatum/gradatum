//! Tests golden pour `parse_tsx_file` (feature `code-typescript`).
//!
//! ## Principe
//!
//! Ces tests vérifient que parse_tsx_file :
//! 1. Extrait correctement les composants React (function_declaration avec JSX).
//! 2. Extrait les hooks (`const [s, setS] = useState()`) comme arrow functions.
//! 3. Extrait les exports (visibility = "pub").
//! 4. Ne fait PAS échouer le parse sur du JSX valide (pas de has_error).
//! 5. Produit les mêmes symboles que parse_typescript_file sur du TS pur (parité node-kinds).
//!
//! ## Non-régression
//!
//! Vérifie que parse_typescript_file (.ts) n'est pas affectée.

#![cfg(feature = "code-typescript")]

use gradatum_ingest::{parse_tsx_file, parse_typescript_file};

// ── Composant React réaliste ──────────────────────────────────────────────────

/// Composant React avec JSX, hooks, et arrow function exportée.
///
/// Conçu pour tester les cas F-61 TSX :
/// - `export function Counter()` → composant React (kind = "fn")
/// - `const increment` = arrow exportée → kind = "fn"
/// - JSX (`<div>`, `<button>`) → ne doit pas faire échouer le parse
/// - `useState` appelé → nœud call_expression (pour futur callgraph)
const SNIPPET_TSX_REACT: &str = r#"import React, { useState } from 'react';

/** Compteur React avec état local. */
export function Counter() {
    const [count, setCount] = useState(0);
    return (
        <div className="counter">
            <span>{count}</span>
            <button onClick={() => setCount(count + 1)}>+</button>
        </div>
    );
}

/** Incrémente une valeur. */
export const increment = (n: number): number => n + 1;

/** Interface de props du Counter. */
export interface CounterProps {
    initialValue?: number;
    label: string;
}

// Arrow function non exportée.
const helper = () => 42;
"#;

/// Composant TSX pur TS sans JSX — pour tester la parité node-kinds TS↔TSX.
const SNIPPET_TSX_PURE_TS: &str =
    "export function add(a: number, b: number): number { return a + b; }\n";

// ── Test 1 : parse_tsx_file ne renvoie pas None ───────────────────────────────

/// parse_tsx_file retourne Some(...) sur un composant React valide.
#[test]
fn tsx_parse_returns_ok() {
    let result = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, true);
    assert!(
        result.is_ok(),
        "parse_tsx_file doit retourner Ok sur un composant React valide, obtenu : {:?}",
        result
    );
}

// ── Test 2 : le composant React est extrait ───────────────────────────────────

/// Le composant Counter (function_declaration exportée) est extrait comme kind = "fn".
#[test]
fn tsx_extracts_react_component() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, false).expect("parse ok");

    let counter = symbols
        .iter()
        .find(|s| s.qualified_name == "Counter")
        .expect("le composant 'Counter' doit être extrait");

    assert_eq!(counter.kind, "fn", "Counter est un kind = fn");
    assert_eq!(
        counter.visibility, "pub",
        "Counter est exporté → visibility = pub"
    );
    assert_eq!(
        counter.source_path, "src/Counter.tsx",
        "source_path correctement propagé"
    );
}

// ── Test 3 : l'arrow function exportée est extraite ──────────────────────────

/// `export const increment = (n: number): number => n + 1` → kind = "fn", visibility = "pub".
#[test]
fn tsx_extracts_exported_arrow_function() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, false).expect("parse ok");

    let inc = symbols
        .iter()
        .find(|s| s.qualified_name == "increment")
        .expect("'increment' arrow function doit être extraite");

    assert_eq!(inc.kind, "fn");
    assert_eq!(inc.visibility, "pub");
}

// ── Test 4 : l'interface exportée est extraite ────────────────────────────────

/// `export interface CounterProps` → kind = "type", visibility = "pub".
#[test]
fn tsx_extracts_interface() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, true).expect("parse ok");

    let iface = symbols
        .iter()
        .find(|s| s.qualified_name == "CounterProps")
        .expect("'CounterProps' interface doit être extraite");

    assert_eq!(iface.kind, "type");
    assert_eq!(iface.visibility, "pub");
}

// ── Test 5 : pas de symboles JSX parasites ────────────────────────────────────

/// Les nœuds JSX (div, button, span) ne doivent pas générer de faux symboles.
///
/// Seuls les items TS légitimes (fn, class, interface, arrow) sont extraits.
#[test]
fn tsx_no_jsx_noise_in_symbols() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, true).expect("parse ok");

    // Aucun symbole dont le nom ressemble à un tag HTML.
    for sym in &symbols {
        assert!(
            !sym.qualified_name.starts_with('<'),
            "tag JSX '<...>' ne doit pas apparaître comme symbole : {:?}",
            sym.qualified_name
        );
        // Les noms de tags courants ne doivent pas apparaître.
        let name = sym.qualified_name.as_str();
        assert!(
            !["div", "span", "button", "className"].contains(&name),
            "tag HTML/JSX '{name}' ne doit pas être un symbole extrait"
        );
    }
}

// ── Test 6 : include_private=false exclut helper ──────────────────────────────

/// `const helper = () => 42` (non exporté) → exclu si include_private=false.
#[test]
fn tsx_include_private_false_excludes_helper() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, false).expect("parse ok");

    let has_helper = symbols.iter().any(|s| s.qualified_name == "helper");
    assert!(
        !has_helper,
        "'helper' non exportée ne doit pas apparaître quand include_private=false"
    );
}

/// `const helper = () => 42` (non exporté) → inclus si include_private=true.
#[test]
fn tsx_include_private_true_includes_helper() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, true).expect("parse ok");

    let has_helper = symbols.iter().any(|s| s.qualified_name == "helper");
    assert!(
        has_helper,
        "'helper' non exportée doit apparaître quand include_private=true"
    );
}

// ── Test 7 : parité node-kinds TS↔TSX sur TS pur ─────────────────────────────

/// Sur du TS pur (sans JSX), parse_tsx_file et parse_typescript_file produisent
/// les mêmes symboles (même nombre, mêmes noms, mêmes kinds).
///
/// Prouve que les node-kinds `function_declaration`, `class_declaration`, etc.
/// sont identiques entre LANGUAGE_TYPESCRIPT et LANGUAGE_TSX — seul le JSX diffère.
#[test]
fn tsx_parity_with_ts_parser_on_pure_ts() {
    let ts_symbols = parse_typescript_file("utils.ts", SNIPPET_TSX_PURE_TS, true)
        .expect("parse_typescript_file ok");
    let tsx_symbols =
        parse_tsx_file("utils.tsx", SNIPPET_TSX_PURE_TS, true).expect("parse_tsx_file ok");

    assert_eq!(
        ts_symbols.len(),
        tsx_symbols.len(),
        "même nombre de symboles sur du TS pur : ts={} tsx={}",
        ts_symbols.len(),
        tsx_symbols.len()
    );

    for (ts_sym, tsx_sym) in ts_symbols.iter().zip(tsx_symbols.iter()) {
        assert_eq!(
            ts_sym.qualified_name, tsx_sym.qualified_name,
            "qualified_name doit correspondre"
        );
        assert_eq!(ts_sym.kind, tsx_sym.kind, "kind doit correspondre");
        assert_eq!(
            ts_sym.visibility, tsx_sym.visibility,
            "visibility doit correspondre"
        );
    }
}

// ── Test 8 : non-régression parse_typescript_file ────────────────────────────

/// Vérifie que l'ajout de parse_tsx_file n'affecte pas parse_typescript_file.
///
/// parse_typescript_file doit continuer à utiliser LANGUAGE_TYPESCRIPT
/// et extraire correctement du TS pur.
#[test]
fn ts_parser_non_regression_after_tsx_addition() {
    let source = "export function handler(req: Request): Response { return new Response(); }\n";
    let symbols = parse_typescript_file("api/handler.ts", source, false)
        .expect("parse_typescript_file doit toujours fonctionner");

    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].qualified_name, "handler");
    assert_eq!(symbols[0].kind, "fn");
    assert_eq!(symbols[0].visibility, "pub");
}

// ── Test 9 : doc-comment JSDoc sur composant TSX ─────────────────────────────

/// Le commentaire JSDoc `/** Compteur React... */` doit être extrait.
#[test]
fn tsx_extracts_jsdoc_on_component() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, true).expect("parse ok");

    let counter = symbols
        .iter()
        .find(|s| s.qualified_name == "Counter")
        .expect("Counter extrait");

    assert!(
        counter.doc_comment.is_some(),
        "Counter doit avoir un doc_comment JSDoc"
    );
    let doc = counter.doc_comment.as_ref().expect("doc_comment Some");
    assert!(
        doc.contains("Compteur") || doc.contains("React"),
        "doc_comment doit contenir le texte JSDoc, obtenu {:?}",
        doc
    );
}

// ── Test 10 : source vide → zéro symboles ────────────────────────────────────

/// parse_tsx_file sur source vide ne panic pas et retourne Ok(vec![]).
#[test]
fn tsx_empty_source_returns_empty() {
    let symbols = parse_tsx_file("empty.tsx", "", true).expect("parse ok");
    assert!(symbols.is_empty(), "source vide = zéro symboles");
}

// ── Test 11 : span propagé ───────────────────────────────────────────────────

/// Vérifie que span (start_line, end_line) est propagé pour les symboles TSX.
#[test]
fn tsx_span_propagated() {
    let symbols = parse_tsx_file("src/Counter.tsx", SNIPPET_TSX_REACT, false).expect("parse ok");

    for sym in &symbols {
        assert!(
            sym.span.is_some(),
            "span doit être Some pour le symbole '{}' dans un fichier TSX",
            sym.qualified_name
        );
        let (start, end) = sym.span.expect("span Some");
        assert!(
            start >= 1,
            "start_line doit être >= 1 pour '{}'",
            sym.qualified_name
        );
        assert!(
            end >= start,
            "end_line >= start_line pour '{}'",
            sym.qualified_name
        );
    }
}

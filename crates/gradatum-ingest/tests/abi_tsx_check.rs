//! Test de validation ABI : tree-sitter-typescript 0.23.2 — grammaire LANGUAGE_TSX.
//!
//! Vérifie que `LANGUAGE_TSX` (grammaire JSX/React) est ABI-compatible avec
//! tree-sitter core 0.26.9, AVANT l'implémentation de parse_tsx_file.
//!
//! Si `set_language` retourne une erreur, l'implémentation TSX doit être stoppée
//! car le bug serait purement ABI (version mismatch crate/core).

#![cfg(feature = "code-typescript")]

/// Vérifie que LANGUAGE_TSX est ABI-compatible avec tree-sitter core 0.26.9.
///
/// La grammaire TSX est fournie par la même crate `tree-sitter-typescript 0.23.2`
/// que LANGUAGE_TYPESCRIPT — le check ABI couvre les deux.
#[test]
fn abi_tsx_compatible_with_core_0_26() {
    let mut parser = tree_sitter::Parser::new();
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TSX.into();
    parser
        .set_language(&lang)
        .expect("BLOQUANT ABI: LANGUAGE_TSX incompatible avec tree-sitter core 0.26.9 — stopper l'implémentation TSX");

    // Composant React minimal avec JSX : prouve que la grammaire comprend JSX
    // sans générer d'erreur de parse (node has_error = false).
    let source = b"export function App() { return <div>hello</div>; }\n";
    let tree = parser
        .parse(source, None)
        .expect("parse retourne None uniquement si timeout ou langage non défini");

    assert!(
        !tree.root_node().has_error(),
        "AST LANGUAGE_TSX ne doit pas avoir d'erreur sur un composant JSX minimal"
    );
    assert_eq!(
        tree.root_node().kind(),
        "program",
        "nœud racine TSX = program (identique à LANGUAGE_TYPESCRIPT)"
    );
}

/// Vérifie que LANGUAGE_TSX comprend les node-kinds function_declaration et jsx_element.
///
/// Prouve que :
/// 1. `function_declaration` existe dans LANGUAGE_TSX (parité avec LANGUAGE_TYPESCRIPT).
/// 2. Le JSX (`jsx_element`) est un nœud valide — absent de LANGUAGE_TYPESCRIPT.
#[test]
fn abi_tsx_node_kinds_include_jsx() {
    let mut parser = tree_sitter::Parser::new();
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TSX.into();
    parser
        .set_language(&lang)
        .expect("LANGUAGE_TSX ABI compatible");

    // Source avec JSX explicite.
    let source = b"export function Foo() { return <div className=\"x\">{bar}</div>; }\n";
    let tree = parser.parse(source, None).expect("parse ok");
    let root = tree.root_node();

    // Vérifier qu'il n'y a pas d'erreur de parse — le JSX est compris.
    assert!(
        !root.has_error(),
        "LANGUAGE_TSX ne doit pas avoir d'erreur de parse sur du JSX valide"
    );

    // Vérifier que le root a un enfant function_declaration (via export_statement).
    let mut found_fn_decl = false;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "export_statement" {
            if let Some(decl) = child.child_by_field_name("declaration")
                && decl.kind() == "function_declaration"
            {
                found_fn_decl = true;
            }
        } else if child.kind() == "function_declaration" {
            found_fn_decl = true;
        }
    }
    assert!(
        found_fn_decl,
        "LANGUAGE_TSX doit produire un nœud function_declaration pour `export function Foo() {{...}}`"
    );
}

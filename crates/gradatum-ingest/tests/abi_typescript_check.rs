//! Test de validation ABI : tree-sitter-typescript 0.23.2 avec tree-sitter core 0.26.9.
//!
//! Ce test est exécuté AVANT l'implémentation de TypeScriptParser pour vérifier
//! que la shim `tree-sitter-language` est compatible entre les deux versions.
//! Si `set_language` retourne une erreur, l'implémentation doit être stoppée.

#![cfg(feature = "code-typescript")]

/// Vérifie que tree-sitter-typescript 0.23.2 est ABI-compatible avec tree-sitter core 0.26.9.
///
/// Le mécanisme de validation : `set_language` retourne `Err` si la version ABI
/// de la grammaire (LANGUAGE.version) est hors de la plage acceptée par le core
/// (TREE_SITTER_MIN_COMPATIBLE_LANGUAGE_VERSION..=TREE_SITTER_LANGUAGE_VERSION).
#[test]
fn abi_typescript_compatible_with_core_0_26() {
    let mut parser = tree_sitter::Parser::new();
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    parser
        .set_language(&lang)
        .expect("BLOQUANT ABI: tree-sitter-typescript 0.23.2 incompatible avec tree-sitter core 0.26.9 — stopper l'implémentation");
    let source = b"const x = 1;\n";
    let tree = parser
        .parse(source, None)
        .expect("parse retourne None uniquement si timeout ou langage non défini");
    assert!(
        !tree.root_node().has_error(),
        "AST ne doit pas avoir d'erreur sur `const x = 1`"
    );
    // Vérifier la structure de base de l'AST
    let root = tree.root_node();
    assert_eq!(root.kind(), "program", "nœud racine TypeScript = program");
}

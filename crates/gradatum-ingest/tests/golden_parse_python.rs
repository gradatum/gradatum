//! Tests golden pour `parse_python_file` (feature `code-python`).
//!
//! ## Principe
//!
//! Ces tests utilisent un snippet Python représentatif pour vérifier :
//! 1. Les classes extraites (kind="class", qualified_name, docstring).
//! 2. Les méthodes extraites (kind="method", qualified_name, visibilité).
//! 3. Les fonctions top-level (kind="fn", qualified_name, signature).
//! 4. La convention de visibilité Python (`_`-prefix = privé).
//! 5. Les docstrings extraites sur les items documentés.
//! 6. L'idempotence : même contenu parsé deux fois → mêmes symboles.
//!
//! ## Non-extraits (par design)
//!
//! - Assignments module-level (CONSTANT = 42) : aucun `SymbolKind` adapté
//!   dans `DerivedSymbol.kind` (les kinds stricts sont "fn"/"class"/"method") —
//!   skip documenté (accuracy > coverage).
//! - Deps Python : laissé `vec![]` pour l'incrément 1 (TODO F-61 inc2).

#![cfg(feature = "code-python")]

use gradatum_ingest::parse_python_file;

/// Snippet Python réaliste couvrant les cas principaux.
const SNIPPET_GOLDEN: &str = r#""""Module docstring."""

CONSTANT = 42

class Animal:
    """Represents an animal."""

    def __init__(self, name: str, age: int) -> None:
        """Initialize the animal."""
        self.name = name
        self.age = age

    def speak(self) -> str:
        """Make the animal speak."""
        raise NotImplementedError

    def _internal(self) -> None:
        """Private helper."""
        pass

class Dog(Animal):
    """A dog."""

    def speak(self) -> str:
        """Bark."""
        return "Woof!"

def standalone_function(x: int, y: int = 0) -> int:
    """Add two numbers."""
    return x + y

def _private_func() -> None:
    pass
"#;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn symbols_by_name<'a>(
    symbols: &'a [gradatum_ingest::DerivedSymbol],
    name: &str,
) -> Vec<&'a gradatum_ingest::DerivedSymbol> {
    symbols
        .iter()
        .filter(|s| s.qualified_name == name)
        .collect()
}

fn find_sym<'a>(
    symbols: &'a [gradatum_ingest::DerivedSymbol],
    name: &str,
) -> Option<&'a gradatum_ingest::DerivedSymbol> {
    symbols.iter().find(|s| s.qualified_name == name)
}

// ── Golden test 1 : classe Animal extraite comme type ────────────────────────

/// Vérifie que `Animal` est extrait comme `class` avec docstring.
#[test]
fn golden_extracts_animal_as_class() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let animal = find_sym(&symbols, "Animal").expect("Animal doit être extrait");
    assert_eq!(animal.kind, "class", "Animal kind = class");
    assert_eq!(
        animal.visibility, "pub",
        "Animal est public (pas de _-prefix)"
    );
    assert!(
        animal.doc_comment.is_some(),
        "Animal doit avoir une docstring"
    );
    let doc = animal.doc_comment.as_ref().unwrap();
    assert!(
        doc.contains("Represents an animal"),
        "docstring Animal : attendu 'Represents an animal', obtenu {:?}",
        doc
    );
}

// ── Golden test 2 : classe Dog avec base (superclasse) ───────────────────────

/// Vérifie que `Dog` est extrait comme `class` avec docstring.
#[test]
fn golden_extracts_dog_as_class_with_base() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let dog = find_sym(&symbols, "Dog").expect("Dog doit être extrait");
    assert_eq!(dog.kind, "class", "Dog kind = class");
    assert_eq!(dog.visibility, "pub", "Dog est public");
    assert!(dog.doc_comment.is_some(), "Dog doit avoir une docstring");
    // La signature peut contenir la base si extraite, ou None — les deux sont acceptables.
    // Ce qui est requis : kind=class + nom correct.
}

// ── Golden test 3 : méthodes de Animal ───────────────────────────────────────

/// Vérifie que `__init__`, `speak` et `_internal` sont extraits comme méthodes.
#[test]
fn golden_extracts_animal_methods() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    // __init__ : méthode publique (convention Python — dunder = pub)
    let init = find_sym(&symbols, "Animal::__init__").expect("Animal::__init__ doit être extrait");
    assert_eq!(init.kind, "method", "Animal::__init__ kind = method");
    assert_eq!(init.visibility, "pub", "dunder = pub par convention");
    assert!(
        init.doc_comment.is_some(),
        "Animal::__init__ doit avoir une docstring"
    );

    // speak : méthode publique
    let speak_candidates = symbols_by_name(&symbols, "Animal::speak");
    assert!(
        !speak_candidates.is_empty(),
        "Animal::speak doit être extrait"
    );
    let speak = speak_candidates[0];
    assert_eq!(speak.kind, "method", "Animal::speak kind = method");
    assert_eq!(speak.visibility, "pub", "Animal::speak est public");

    // _internal : méthode privée (_-prefix)
    let internal =
        find_sym(&symbols, "Animal::_internal").expect("Animal::_internal doit être extrait");
    assert_eq!(internal.kind, "method", "Animal::_internal kind = method");
    assert_eq!(
        internal.visibility, "priv",
        "Animal::_internal est privé (_-prefix)"
    );
}

// ── Golden test 4 : méthode Dog::speak ───────────────────────────────────────

/// Vérifie que `Dog::speak` est extrait comme méthode.
#[test]
fn golden_extracts_dog_method() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let dog_speak = find_sym(&symbols, "Dog::speak").expect("Dog::speak doit être extrait");
    assert_eq!(dog_speak.kind, "method");
    assert_eq!(dog_speak.visibility, "pub");
}

// ── Golden test 5 : fonction top-level standalone_function ───────────────────

/// Vérifie que `standalone_function` est extrait comme fn avec params et docstring.
#[test]
fn golden_extracts_standalone_function() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let func =
        find_sym(&symbols, "standalone_function").expect("standalone_function doit être extrait");
    assert_eq!(func.kind, "fn", "standalone_function kind = fn");
    assert_eq!(func.visibility, "pub", "standalone_function est public");

    // Signature : doit contenir les paramètres
    assert!(
        func.signature.is_some(),
        "standalone_function doit avoir une signature"
    );
    let sig = func.signature.as_ref().unwrap();
    assert!(
        sig.contains("x") || sig.contains("int"),
        "signature doit contenir au moins 'x' ou 'int', obtenu {:?}",
        sig
    );

    // Docstring
    assert!(
        func.doc_comment.is_some(),
        "standalone_function doit avoir une docstring"
    );
    let doc = func.doc_comment.as_ref().unwrap();
    assert!(
        doc.contains("Add two numbers"),
        "docstring : attendu 'Add two numbers', obtenu {:?}",
        doc
    );
}

// ── Golden test 6 : fonction privée _private_func ────────────────────────────

/// Vérifie que `_private_func` est extrait comme fn privée.
#[test]
fn golden_extracts_private_function() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("parse doit réussir");

    let priv_fn = find_sym(&symbols, "_private_func")
        .expect("_private_func doit être extrait (include_private=true)");
    assert_eq!(priv_fn.kind, "fn");
    assert_eq!(
        priv_fn.visibility, "priv",
        "_private_func doit être marqué priv (_-prefix)"
    );
}

// ── Golden test 7 : filtre visibilité (include_private=false) ────────────────

/// Vérifie que les items privés sont exclus quand `include_private=false`.
#[test]
fn golden_visibility_filter_excludes_private() {
    let symbols =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, false).expect("parse doit réussir");

    // _private_func ne doit pas apparaître
    let priv_fn = find_sym(&symbols, "_private_func");
    assert!(
        priv_fn.is_none(),
        "_private_func ne doit pas être extrait quand include_private=false"
    );

    // _internal ne doit pas apparaître
    let internal = find_sym(&symbols, "Animal::_internal");
    assert!(
        internal.is_none(),
        "Animal::_internal ne doit pas être extrait quand include_private=false"
    );

    // Les items publics doivent toujours être présents
    assert!(
        find_sym(&symbols, "Animal").is_some(),
        "Animal doit être extrait même avec include_private=false"
    );
    assert!(
        find_sym(&symbols, "standalone_function").is_some(),
        "standalone_function doit être extrait même avec include_private=false"
    );
}

// ── Golden test 8 : idempotence ──────────────────────────────────────────────

/// Même source parsée deux fois → mêmes noms de symboles.
#[test]
fn golden_idempotent_parse() {
    let symbols_1 =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("premier parse ok");
    let symbols_2 =
        parse_python_file("src/example.py", SNIPPET_GOLDEN, true).expect("deuxième parse ok");

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

// ── Golden test 9 : source_path propagé ──────────────────────────────────────

/// Vérifie que `source_path` est correctement propagé dans tous les symboles.
#[test]
fn golden_source_path_propagated() {
    let path = "myproject/main.py";
    let symbols = parse_python_file(path, SNIPPET_GOLDEN, true).expect("parse ok");

    for sym in &symbols {
        assert_eq!(
            sym.source_path, path,
            "source_path incorrect pour '{}'",
            sym.qualified_name
        );
    }
}

// ── Golden test 10 : snippet minimal ─────────────────────────────────────────

/// Vérifie que le parser gère correctement une source minimale.
#[test]
fn golden_minimal_source() {
    let source = "x = 1\n";
    let symbols = parse_python_file("test.py", source, true).expect("parse ok");
    // Un simple assignment n'est pas un symbole extractible (accuracy > coverage)
    assert!(
        symbols.is_empty(),
        "assignment seul ne produit aucun symbole, got: {:?}",
        symbols
            .iter()
            .map(|s| &s.qualified_name)
            .collect::<Vec<_>>()
    );
}

// ── Golden test 11 : source vide ─────────────────────────────────────────────

/// Vérifie que le parser gère une source vide sans panic.
#[test]
fn golden_empty_source() {
    let symbols = parse_python_file("empty.py", "", true).expect("parse ok");
    assert!(symbols.is_empty(), "source vide = zéro symboles");
}

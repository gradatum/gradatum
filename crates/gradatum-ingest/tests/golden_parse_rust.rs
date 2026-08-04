//! Tests golden pour `parse_rust_file` (feature `code-rust`).
//!
//! ## Principe
//!
//! Ces tests utilisent des snippets Rust représentatifs (inline dans le test) qui simulent
//! la structure typique du codebase gradatum. Ils vérifient :
//! 1. Les symboles extraits (kind, qualified_name, signature).
//! 2. La stabilité du note_id (même parse → même id).
//! 3. L'idempotence : même contenu parsé deux fois → mêmes symboles.
//! 4. La gestion des duplicates ambigus.
//!
//! ## Sources réelles
//!
//! Les 3 derniers tests utilisent le contenu des fichiers RÉELS de gradatum
//! (lus dynamiquement depuis le disque) pour valider le parser sur du vrai code.

#![cfg(feature = "code-rust")]

use gradatum_ingest::{build_derived_notes, content_hash_source, parse_rust_file};

// ── Golden test 1 : snippet struct + fn pub ──────────────────────────────────

const SNIPPET_BASIC: &str = r#"
//! Module de test pour le parser.

/// Struct principale du vault.
pub struct VaultId(pub String);

impl VaultId {
    /// Construit un VaultId.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Retourne la str.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn private_helper(&self) -> bool {
        true
    }
}

/// Parse un fichier.
pub fn parse_file(path: &str) -> Vec<String> {
    vec![]
}

fn internal_func() -> u32 {
    42
}

pub const MAX_TOKENS: usize = 800;
"#;

/// Vérifie que les symboles publics sont extraits et les privés ignorés.
#[test]
fn golden_basic_extracts_public_symbols() {
    let symbols = parse_rust_file("src/lib.rs", SNIPPET_BASIC, false).expect("parse ok");

    let kinds_names: Vec<(&str, &str)> = symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.qualified_name.as_str()))
        .collect();

    // Doit contenir VaultId struct.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "struct" && *n == "VaultId"),
        "VaultId struct manquante. Trouvé: {kinds_names:?}"
    );

    // Doit contenir impl VaultId.
    assert!(
        kinds_names.iter().any(|(k, _)| *k == "impl"),
        "impl block manquant. Trouvé: {kinds_names:?}"
    );

    // Doit contenir les méthodes publiques.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "method" && *n == "VaultId::new"),
        "VaultId::new manquante. Trouvé: {kinds_names:?}"
    );
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "method" && *n == "VaultId::as_str"),
        "VaultId::as_str manquante. Trouvé: {kinds_names:?}"
    );

    // NE doit PAS contenir private_helper.
    assert!(
        !kinds_names
            .iter()
            .any(|(_, n)| *n == "VaultId::private_helper"),
        "private_helper ne devrait pas être extrait. Trouvé: {kinds_names:?}"
    );

    // Doit contenir parse_file fn publique.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "fn" && *n == "parse_file"),
        "parse_file manquante. Trouvé: {kinds_names:?}"
    );

    // NE doit PAS contenir internal_func.
    assert!(
        !kinds_names.iter().any(|(_, n)| *n == "internal_func"),
        "internal_func ne devrait pas être extrait. Trouvé: {kinds_names:?}"
    );

    // Doit contenir MAX_TOKENS const.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "const" && *n == "MAX_TOKENS"),
        "MAX_TOKENS manquante. Trouvé: {kinds_names:?}"
    );
}

/// Vérifie que le note_id est stable (même parse → même id).
#[test]
fn golden_note_id_stability() {
    let vault_id = "code-gradatum";
    let symbols1 = parse_rust_file("src/lib.rs", SNIPPET_BASIC, false).expect("parse 1");
    let symbols2 = parse_rust_file("src/lib.rs", SNIPPET_BASIC, false).expect("parse 2");

    let notes1 = build_derived_notes(vault_id, symbols1);
    let notes2 = build_derived_notes(vault_id, symbols2);

    assert_eq!(
        notes1.len(),
        notes2.len(),
        "même contenu doit produire le même nombre de notes"
    );

    for (n1, n2) in notes1.iter().zip(notes2.iter()) {
        assert_eq!(
            n1.id,
            n2.id,
            "note_id doit être stable pour '{}': {:?} != {:?}",
            n1.title.as_deref().unwrap_or("?"),
            n1.id,
            n2.id
        );
    }
}

// ── Golden test 2 : snippet enum + trait + impl Trait for Type ───────────────

const SNIPPET_TRAIT_IMPL: &str = r#"
/// Statut d'une note.
pub enum NoteStatus {
    Live,
    Staging,
    Forgotten,
}

/// Trait principal d'indexation.
pub trait IndexStore {
    /// Cherche les notes.
    fn search(&self, query: &str) -> Vec<String>;
}

/// Implémentation in-memory.
pub struct InMemoryIndex;

impl IndexStore for InMemoryIndex {
    fn search(&self, query: &str) -> Vec<String> {
        vec![query.to_string()]
    }
}
"#;

/// Vérifie l'extraction des enums, traits et impl Trait for Type.
#[test]
fn golden_trait_impl_extraction() {
    let symbols = parse_rust_file("src/index.rs", SNIPPET_TRAIT_IMPL, false).expect("parse ok");
    let kinds_names: Vec<(&str, &str)> = symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.qualified_name.as_str()))
        .collect();

    // NoteStatus enum.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "enum" && *n == "NoteStatus"),
        "NoteStatus enum manquante. Trouvé: {kinds_names:?}"
    );

    // IndexStore trait.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "trait" && *n == "IndexStore"),
        "IndexStore trait manquant. Trouvé: {kinds_names:?}"
    );

    // InMemoryIndex struct.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "struct" && *n == "InMemoryIndex"),
        "InMemoryIndex struct manquante. Trouvé: {kinds_names:?}"
    );

    // impl IndexStore for InMemoryIndex.
    assert!(
        kinds_names.iter().any(|(k, _)| *k == "impl"),
        "impl block manquant. Trouvé: {kinds_names:?}"
    );
}

// ── Golden test 3 : snippet avec duplicates ambigus ──────────────────────────

const SNIPPET_DUPLICATES: &str = r#"
pub fn duplicate_fn() -> u32 { 1 }
pub fn duplicate_fn() -> u32 { 2 }
pub fn unique_fn() -> bool { true }
"#;

/// Vérifie que les duplicates sont omis (accuracy > coverage).
#[test]
fn golden_duplicates_omitted() {
    let symbols = parse_rust_file("src/dup.rs", SNIPPET_DUPLICATES, false).expect("parse ok");
    let notes = build_derived_notes("code-gradatum", symbols);

    // duplicate_fn apparaît 2x → les deux marqués ambiguous → omis.
    let dup_count = notes
        .iter()
        .filter(|n| n.title.as_deref() == Some("duplicate_fn"))
        .count();
    assert_eq!(
        dup_count, 0,
        "duplicate_fn doit être omis (ambiguous). count={dup_count}"
    );

    // unique_fn doit être présente.
    let unique_count = notes
        .iter()
        .filter(|n| n.title.as_deref() == Some("unique_fn"))
        .count();
    assert_eq!(
        unique_count, 1,
        "unique_fn doit être présente. count={unique_count}"
    );
}

// ── Golden test 4 : idempotence content_hash_source ──────────────────────────

/// content_hash_source est déterministe.
#[test]
fn golden_content_hash_deterministic() {
    let content = SNIPPET_BASIC.as_bytes();
    let h1 = content_hash_source(content);
    let h2 = content_hash_source(content);
    assert_eq!(h1, h2, "content_hash_source doit être déterministe");
    assert_eq!(h1.len(), 64, "SHA-256 hex = 64 chars");
}

// ── Golden test 5 : body_text cap ≤ 60 lignes ────────────────────────────────

/// body_text est toujours ≤ 60 lignes.
#[test]
fn golden_body_text_cap_60_lines() {
    let symbols = parse_rust_file("src/lib.rs", SNIPPET_BASIC, false).expect("parse ok");
    let notes = build_derived_notes("code-gradatum", symbols);
    for note in &notes {
        let line_count = note.body_text.lines().count();
        assert!(
            line_count <= 60,
            "body_text dépasse 60 lignes pour '{}': {line_count}",
            note.title.as_deref().unwrap_or("?")
        );
    }
}

// ── Golden test 6 : fichiers RÉELS de gradatum ───────────────────────────────

/// Parse le fichier scope.rs de gradatum-core — vérifie VaultId struct.
#[test]
fn golden_real_scope_rs() {
    let content = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/gradatum-core/src/scope.rs"
    ))
    .expect("lire scope.rs");

    let symbols = parse_rust_file("crates/gradatum-core/src/scope.rs", &content, false)
        .expect("parse scope.rs");

    let kinds_names: Vec<(&str, &str)> = symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.qualified_name.as_str()))
        .collect();

    // VaultId doit être présent.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "struct" && *n == "VaultId"),
        "VaultId struct manquante dans scope.rs. Trouvé: {kinds_names:?}"
    );

    // VaultId::new doit être présente.
    assert!(
        kinds_names
            .iter()
            .any(|(k, n)| *k == "method" && *n == "VaultId::new"),
        "VaultId::new manquante dans scope.rs. Trouvé: {kinds_names:?}"
    );

    // Stabilité des note_ids.
    let notes1 = build_derived_notes("code-gradatum", symbols.clone());
    let notes2 = build_derived_notes("code-gradatum", symbols);
    for (n1, n2) in notes1.iter().zip(notes2.iter()) {
        assert_eq!(n1.id, n2.id, "note_id scope.rs instable");
    }
}

/// Parse le fichier identity.rs de gradatum-core — vérifie NoteId.
#[test]
fn golden_real_identity_rs() {
    let content = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/gradatum-core/src/identity.rs"
    ))
    .expect("lire identity.rs");

    let symbols = parse_rust_file("crates/gradatum-core/src/identity.rs", &content, false)
        .expect("parse identity.rs");

    // NoteId struct doit être présent.
    let has_note_id = symbols
        .iter()
        .any(|s| s.kind == "struct" && s.qualified_name == "NoteId");
    assert!(has_note_id, "NoteId struct manquante dans identity.rs");

    // Aucun symbole ne dépasse 60 lignes.
    let notes = build_derived_notes("code-gradatum", symbols);
    for note in &notes {
        let lines = note.body_text.lines().count();
        assert!(
            lines <= 60,
            "body_text > 60 lignes pour '{}'",
            note.title.as_deref().unwrap_or("?")
        );
    }
}

/// Parse le fichier sqlite.rs (gros fichier) — vérifie scalabilité et pas de panique.
#[test]
fn golden_real_sqlite_rs_no_panic() {
    let content = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/gradatum-index/src/sqlite.rs"
    ))
    .expect("lire sqlite.rs");

    let symbols = parse_rust_file("crates/gradatum-index/src/sqlite.rs", &content, false)
        .expect("parse sqlite.rs");

    // Au moins 1 symbole doit être extrait (fichier non-vide).
    assert!(
        !symbols.is_empty(),
        "sqlite.rs doit produire au moins 1 symbole"
    );

    // Stabilité note_ids.
    let notes1 = build_derived_notes("code-gradatum", symbols.clone());
    let notes2 = build_derived_notes("code-gradatum", symbols);
    assert_eq!(
        notes1.len(),
        notes2.len(),
        "sqlite.rs: nombre de notes instable"
    );
    for (n1, n2) in notes1.iter().zip(notes2.iter()) {
        assert_eq!(
            n1.id,
            n2.id,
            "note_id sqlite.rs instable pour '{}'",
            n1.title.as_deref().unwrap_or("?")
        );
    }
}

// ── Tests discriminants — feature Visibility ─────────────────────────────────

/// Snippet avec fonctions publiques ET privées, pour tester le filtrage de visibilité.
const SNIPPET_VISIBILITY: &str = r#"
/// Fonction publique attendue dans tous les modes.
pub fn public_fn(x: u32) -> u32 { x }

/// Fonction privée — présente en mode All, absente en mode Pub.
fn private_fn(x: u32) -> u32 { x + 1 }

/// Constante publique.
pub const PUB_CONST: u32 = 42;

/// Constante privée — présente en mode All, absente en mode Pub.
const PRIV_CONST: u32 = 7;

/// Struct publique.
pub struct PubStruct;

/// Struct privée — présente en mode All, absente en mode Pub.
struct PrivStruct;

impl PubStruct {
    /// Méthode publique.
    pub fn pub_method(&self) -> u32 { 0 }

    /// Méthode privée — présente en mode All, absente en mode Pub.
    fn priv_method(&self) -> u32 { 1 }
}
"#;

/// Test discriminant V1 — mode Pub (false) : les items privés sont ABSENTS.
///
/// Critère d'acceptation : `private_fn`, `PRIV_CONST`, `PrivStruct` et `priv_method`
/// ne doivent pas figurer dans les symboles extraits en mode Pub.
#[test]
fn visibility_v1_pub_mode_excludes_private_items() {
    let symbols = parse_rust_file("src/lib.rs", SNIPPET_VISIBILITY, false).expect("parse ok");

    let names: Vec<&str> = symbols.iter().map(|s| s.qualified_name.as_str()).collect();

    // Items publics attendus.
    assert!(
        names.contains(&"public_fn"),
        "V1: public_fn manquante en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PUB_CONST"),
        "V1: PUB_CONST manquante en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PubStruct"),
        "V1: PubStruct manquante en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PubStruct::pub_method"),
        "V1: PubStruct::pub_method manquante en mode Pub. Trouvé: {names:?}"
    );

    // Items privés — doivent être ABSENTS en mode Pub.
    assert!(
        !names.contains(&"private_fn"),
        "V1: private_fn ne doit PAS être présente en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        !names.contains(&"PRIV_CONST"),
        "V1: PRIV_CONST ne doit PAS être présente en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        !names.contains(&"PrivStruct"),
        "V1: PrivStruct ne doit PAS être présente en mode Pub. Trouvé: {names:?}"
    );
    assert!(
        !names.contains(&"PubStruct::priv_method"),
        "V1: PubStruct::priv_method ne doit PAS être présente en mode Pub. Trouvé: {names:?}"
    );
}

/// Test discriminant V2 — mode All (true) : les items privés sont PRÉSENTS.
///
/// Critère d'acceptation : `private_fn`, `PRIV_CONST`, `PrivStruct` et `priv_method`
/// doivent figurer dans les symboles extraits en mode All.
#[test]
fn visibility_v2_all_mode_includes_private_items() {
    let symbols = parse_rust_file("src/lib.rs", SNIPPET_VISIBILITY, true).expect("parse ok");

    let names: Vec<&str> = symbols.iter().map(|s| s.qualified_name.as_str()).collect();

    // Items publics toujours présents.
    assert!(
        names.contains(&"public_fn"),
        "V2: public_fn manquante en mode All. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PubStruct::pub_method"),
        "V2: PubStruct::pub_method manquante en mode All. Trouvé: {names:?}"
    );

    // Items privés — doivent être PRÉSENTS en mode All.
    assert!(
        names.contains(&"private_fn"),
        "V2: private_fn DOIT être présente en mode All. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PRIV_CONST"),
        "V2: PRIV_CONST DOIT être présente en mode All. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PrivStruct"),
        "V2: PrivStruct DOIT être présente en mode All. Trouvé: {names:?}"
    );
    assert!(
        names.contains(&"PubStruct::priv_method"),
        "V2: PubStruct::priv_method DOIT être présente en mode All. Trouvé: {names:?}"
    );
}

/// Test discriminant V3 — champ visibility sur les symboles extraits.
///
/// Critère d'acceptation : symboles publics → `visibility="pub"`,
/// symboles privés → `visibility="priv"`.
#[test]
fn visibility_v3_visibility_field_on_symbols() {
    // Mode All pour avoir les deux types dans le même parse.
    let symbols = parse_rust_file("src/lib.rs", SNIPPET_VISIBILITY, true).expect("parse ok");

    // public_fn → "pub"
    let pub_sym = symbols
        .iter()
        .find(|s| s.qualified_name == "public_fn")
        .expect("V3: public_fn doit être présente");
    assert_eq!(
        pub_sym.visibility, "pub",
        "V3: public_fn doit avoir visibility='pub', obtenu '{}'",
        pub_sym.visibility
    );

    // private_fn → "priv"
    let priv_sym = symbols
        .iter()
        .find(|s| s.qualified_name == "private_fn")
        .expect("V3: private_fn doit être présente en mode All");
    assert_eq!(
        priv_sym.visibility, "priv",
        "V3: private_fn doit avoir visibility='priv', obtenu '{}'",
        priv_sym.visibility
    );

    // PubStruct → "pub"
    let pub_struct = symbols
        .iter()
        .find(|s| s.qualified_name == "PubStruct" && s.kind == "struct")
        .expect("V3: PubStruct doit être présente");
    assert_eq!(
        pub_struct.visibility, "pub",
        "V3: PubStruct doit avoir visibility='pub', obtenu '{}'",
        pub_struct.visibility
    );

    // PrivStruct → "priv"
    let priv_struct = symbols
        .iter()
        .find(|s| s.qualified_name == "PrivStruct" && s.kind == "struct")
        .expect("V3: PrivStruct doit être présente en mode All");
    assert_eq!(
        priv_struct.visibility, "priv",
        "V3: PrivStruct doit avoir visibility='priv', obtenu '{}'",
        priv_struct.visibility
    );
}

// ── Tests discriminants — feature include_body (span capture) ────────────────

/// Snippet avec une seule fonction publique sur des lignes précises.
/// La première ligne est vide (commentaire de fichier) pour forcer start_line > 1.
const SNIPPET_SPAN: &str = r#"// fichier de test
// deuxième ligne commentaire
pub fn span_fn(x: u32) -> u32 {
    x + 1
}
"#;

/// Test S1 — span capturé et 1-based.
///
/// Pour `span_fn` dans SNIPPET_SPAN : la fonction commence à la ligne 3 et se termine à
/// la ligne 5 (les 2 premières lignes sont des commentaires). Le span doit être `(3, 5)`.
#[test]
fn span_s1_captured_and_one_based() {
    let symbols = parse_rust_file("src/span.rs", SNIPPET_SPAN, false).expect("parse ok");
    let fn_sym = symbols
        .iter()
        .find(|s| s.qualified_name == "span_fn")
        .expect("S1: span_fn attendue");

    let span = fn_sym.span.expect("S1: span doit être Some pour span_fn");
    assert!(
        span.0 >= 1,
        "S1: start_line doit être ≥ 1 (1-based), obtenu {}",
        span.0
    );
    assert!(
        span.0 <= span.1,
        "S1: start_line ({}) doit être ≤ end_line ({})",
        span.0,
        span.1
    );
    // La fonction occupe 3 lignes : start à ligne 3, end à ligne 5.
    assert_eq!(
        span,
        (3, 5),
        "S1: span_fn doit avoir span (3, 5) — lignes 3 à 5 du SNIPPET_SPAN"
    );
}

/// Test S2 — slice du fichier via span cohérent avec le contenu.
///
/// On vérifie que les lignes extraites via le span correspondent bien au corps
/// de la fonction dans le snippet source.
#[test]
fn span_s2_slice_matches_function_body() {
    let symbols = parse_rust_file("src/span.rs", SNIPPET_SPAN, false).expect("parse ok");
    let fn_sym = symbols
        .iter()
        .find(|s| s.qualified_name == "span_fn")
        .expect("S2: span_fn attendue");

    let (start, end) = fn_sym.span.expect("S2: span doit être Some");

    // Extraire les lignes correspondantes du snippet (1-based → 0-based).
    let lines: Vec<&str> = SNIPPET_SPAN.lines().collect();
    let sliced = &lines[(start - 1) as usize..=(end - 1) as usize];
    let body = sliced.join("\n");

    assert!(
        body.contains("span_fn"),
        "S2: le slice doit contenir 'span_fn', obtenu: {body:?}"
    );
    assert!(
        body.contains("x + 1"),
        "S2: le slice doit contenir le corps 'x + 1', obtenu: {body:?}"
    );
}

/// Test S3 — span propagé dans CodeSymbolMeta (ronde-trip build_derived_notes).
///
/// Après `build_derived_notes`, les métadonnées `code_meta.span` doivent contenir
/// le span capturé par le parser (round-trip via JSON dans extra_json["cs"]).
#[test]
fn span_s3_propagated_to_code_meta() {
    let symbols = parse_rust_file("src/span.rs", SNIPPET_SPAN, false).expect("parse ok");
    let notes = build_derived_notes("code-test", symbols);

    let span_note = notes
        .iter()
        .find(|n| n.title.as_deref() == Some("span_fn"))
        .expect("S3: note span_fn attendue");

    let meta = span_note
        .code_meta
        .as_ref()
        .expect("S3: code_meta doit être Some");

    let span = meta
        .span
        .expect("S3: code_meta.span doit être Some pour span_fn");
    assert_eq!(
        span,
        (3, 5),
        "S3: code_meta.span doit être (3, 5) — propagé depuis DerivedSymbol"
    );
}

// ── Fix A1 : troncature char-safe sur signatures contenant des codepoints multi-byte ──

/// Un fichier Rust dont le nom de fonction et les paramètres contiennent des
/// caractères multi-byte (accents, emoji) au voisinage de l'octet 120.
///
/// ## Invariant vérifié
///
/// `parse_rust_file` ne doit pas paniquer, et la signature extraite doit être
/// une str UTF-8 valide (un simple `String::len()` suffit à le confirmer car Rust
/// interdit les String non-UTF8).
///
/// ## Pourquoi c'était un bug
///
/// L'ancien code faisait `&raw[..120]` (slice par bytes). Si l'octet 120 tombait
/// au milieu d'un codepoint multi-byte (ex. `é` = 2 bytes, `🦀` = 4 bytes), le
/// slice était invalide et `str::from_utf8` ou le formattage paniquait.
/// `str::floor_char_boundary(120)` (stable 1.91) ferait ça nativement, mais le MSRV
/// du projet est 1.88 : le code utilise `char_indices().find(|(i, _)| *i >= 120)` à la
/// place — l'offset de début du 1er char ≥ 120 est une frontière de codepoint valide
/// (cf. `rust_parser.rs`).
#[test]
fn a1_signature_with_multibyte_chars_near_boundary_no_panic() {
    // Construit une fonction dont la signature a > 120 bytes et contient des
    // codepoints multi-byte (accents 2B, euro 3B, crabe emoji 4B) proches de l'octet 120.
    //
    // "é" = \xC3\xA9 (2 bytes), "€" = \xE2\x82\xAC (3 bytes), "🦀" = \xF0\x9F\xA6\x80 (4 bytes).
    //
    // On remplit jusqu'à ce que le paramètre soit > 120 bytes avec un codepoint multi-byte
    // exactement à la frontière (l'emoji à la position ~118 occupe 4 bytes → bytes 118..121).
    let snippet = r#"
pub fn process_données(
    paramètre_un: &str,
    paramètre_deux: usize,
    coût_€_calculé: f64,
    icône_🦀_source: Option<String>,
    valeur_supplémentaire: bool,
) -> Result<String, std::io::Error> {
    todo!()
}
"#;

    // Ne doit pas paniquer.
    let symbols = parse_rust_file("src/multibyte.rs", snippet, false).expect("parse ok");

    // Au moins la fonction `process_données` doit être extraite.
    assert!(
        !symbols.is_empty(),
        "a1: au moins 1 symbole attendu pour la fonction multibyte"
    );

    for sym in &symbols {
        if let Some(sig) = &sym.signature {
            // La signature doit être une String UTF-8 valide — `len()` le prouve
            // (Rust n'autorise pas de String contenant des bytes UTF-8 invalides).
            let _ = sig.len();
            // La troncature ne doit pas introduire le suffixe "…" sur une chaîne tronquée
            // si la chaîne entière tient en ≤ 120 bytes, mais PEUT l'introduire sinon.
            // On vérifie seulement la validité UTF-8 (pas de panic = succès).
        }
    }
}

/// Variante : paramètres exactement à la frontière 120 bytes avec un emoji (4 bytes)
/// qui CHEVAUCHE l'octet 120. L'ancien code slicerait en milieu de codepoint.
#[test]
fn a1_emoji_straddles_byte_120_no_panic() {
    // On construit manuellement une chaîne de paramètres de 123 bytes dont les bytes
    // 117..121 sont l'emoji 🦀 (4 bytes). Le slice naïf `[..120]` cuperait bytes 117..120
    // = 3 premiers bytes de l'emoji → UTF-8 invalide → panic.
    //
    // On l'encapsule dans une déclaration de fonction Rust parsable.
    // "x" * 117 + "🦀" + "y" = 117 + 4 + 1 = 122 bytes (> 120).
    let params_inner = format!("{}{}{}", "x".repeat(117), "🦀", "y");
    assert!(
        params_inner.len() > 120,
        "précondition : params_inner doit être > 120 bytes (len={})",
        params_inner.len()
    );
    // Octet 120 = 3e byte de l'emoji = byte interne → invalide pour slice naïf.
    assert!(
        !params_inner.is_char_boundary(120),
        "précondition : l'octet 120 doit être en milieu de codepoint"
    );

    let snippet = format!(
        r#"pub fn boundary_fn({params_inner}: &str) -> bool {{ true }}"#,
        params_inner = params_inner
    );

    // `parse_rust_file` NE DOIT PAS paniquer.
    let result = parse_rust_file("src/boundary.rs", &snippet, false);
    // Un fichier syntaxiquement invalide peut retourner Ok(vec![]) ou Ok(vec![...]).
    // L'invariant est : pas de panic (le test lui-même paniquait avant le fix).
    match result {
        Ok(_) => {} // succès normal
        Err(e) => panic!("a1_emoji_straddles_byte_120_no_panic: erreur inattendue : {e}"),
    }
}

// ── v0.6.3 : callgraph deps — arêtes d'appel réelles ────────────────────────

/// Snippet représentatif : fn A appelle fn B (call_expression directe),
/// fn C appelle une méthode (method_call_expression).
///
/// Format retenu : callee = segment final du path appelé, en mode `All`
/// (mode `All` = extract_function inclut les fns privées, nécessaire pour voir les deps).
const SNIPPET_CALLGRAPH: &str = r#"
fn helper() -> u32 {
    42
}

fn caller() -> u32 {
    helper()
}

struct Parser {
    input: String,
}

impl Parser {
    fn parse(&self) -> Vec<String> {
        let n = helper();
        self.input.split_whitespace()
            .map(|s| s.to_string())
            .collect()
    }
}

fn chain_caller() -> u32 {
    let x = helper();
    let y = caller();
    x + y
}
"#;

/// `caller_has_dep_on_helper` : fn `caller` appelle `helper` → `deps` de `caller` doit contenir `"helper"`.
#[test]
fn caller_has_dep_on_helper() {
    // Mode all=true pour extraire les fns privées.
    let symbols = parse_rust_file("src/callgraph.rs", SNIPPET_CALLGRAPH, true).expect("parse ok");

    let caller = symbols
        .iter()
        .find(|s| s.kind == "fn" && s.qualified_name == "caller")
        .expect("fn caller doit être extraite en mode all=true");

    assert!(
        caller.deps.contains(&"helper".to_string()),
        "caller doit lister 'helper' dans ses deps (call_expression). deps={:?}",
        caller.deps
    );
}

/// `helper_has_no_fn_deps` : `helper` n'appelle aucune autre fn → deps vide ou sans fn locale.
#[test]
fn helper_has_no_fn_deps() {
    let symbols = parse_rust_file("src/callgraph.rs", SNIPPET_CALLGRAPH, true).expect("parse ok");

    let helper = symbols
        .iter()
        .find(|s| s.kind == "fn" && s.qualified_name == "helper")
        .expect("fn helper doit être extraite");

    assert!(
        !helper.deps.contains(&"caller".to_string()),
        "helper ne doit PAS lister 'caller' dans ses deps. deps={:?}",
        helper.deps
    );
}

/// `chain_caller_has_both_deps` : `chain_caller` appelle `helper` ET `caller` → les deux dans deps.
#[test]
fn chain_caller_has_both_deps() {
    let symbols = parse_rust_file("src/callgraph.rs", SNIPPET_CALLGRAPH, true).expect("parse ok");

    let chain = symbols
        .iter()
        .find(|s| s.kind == "fn" && s.qualified_name == "chain_caller")
        .expect("fn chain_caller doit être extraite");

    assert!(
        chain.deps.contains(&"helper".to_string()),
        "chain_caller doit lister 'helper'. deps={:?}",
        chain.deps
    );
    assert!(
        chain.deps.contains(&"caller".to_string()),
        "chain_caller doit lister 'caller'. deps={:?}",
        chain.deps
    );
}

/// `method_parse_has_dep_on_helper` : la méthode `Parser::parse` appelle `helper`
/// via call_expression → `"helper"` dans ses deps.
///
/// Note : `split_whitespace`, `to_string`, `collect` sont des méthodes stdlib filtrées.
#[test]
fn method_parse_has_dep_on_helper() {
    let symbols = parse_rust_file("src/callgraph.rs", SNIPPET_CALLGRAPH, true).expect("parse ok");

    let parse_method = symbols
        .iter()
        .find(|s| s.kind == "method" && s.qualified_name == "Parser::parse")
        .expect("méthode Parser::parse doit être extraite en mode all=true");

    assert!(
        parse_method.deps.contains(&"helper".to_string()),
        "Parser::parse doit lister 'helper' (call_expression). deps={:?}",
        parse_method.deps
    );
}

/// `callgraph_deps_are_deduplicated` : même callee appelé 2× → une seule entrée dans deps.
#[test]
fn callgraph_deps_are_deduplicated() {
    const SNIPPET_DUP: &str = r#"
fn helper() -> u32 { 42 }

fn double_caller() -> u32 {
    let a = helper();
    let b = helper();
    a + b
}
"#;
    let symbols = parse_rust_file("src/dup.rs", SNIPPET_DUP, true).expect("parse ok");

    let dc = symbols
        .iter()
        .find(|s| s.kind == "fn" && s.qualified_name == "double_caller")
        .expect("fn double_caller doit être extraite");

    let helper_count = dc.deps.iter().filter(|d| d.as_str() == "helper").count();
    assert_eq!(
        helper_count, 1,
        "helper doit apparaître une seule fois dans deps (déduplication). deps={:?}",
        dc.deps
    );
}

/// `reverse_lookup_via_callgraph_deps` : preuve de bout en bout que reverse-deps
/// fonctionne avec les callgraph deps remplis par extract_call_deps.
///
/// Scénario : on parse le SNIPPET_CALLGRAPH, on vérifie que `caller.deps` contient `"helper"`,
/// ce qui signifie que `code_scope_reverse_deps(vault, "helper")` retournerait `caller`.
/// (La couche SQLite est testée dans gradatum-index ; ici on valide uniquement la donnée produite.)
#[test]
fn reverse_lookup_via_callgraph_deps() {
    let symbols = parse_rust_file("src/callgraph.rs", SNIPPET_CALLGRAPH, true).expect("parse ok");

    // Collecter tous les symboles qui listent "helper" dans leurs deps.
    let callers_of_helper: Vec<&str> = symbols
        .iter()
        .filter(|s| s.deps.contains(&"helper".to_string()))
        .map(|s| s.qualified_name.as_str())
        .collect();

    // "caller", "chain_caller", "Parser::parse" appellent tous helper.
    assert!(
        callers_of_helper.contains(&"caller"),
        "reverse-deps('helper') doit trouver 'caller'. trouve={callers_of_helper:?}"
    );
    assert!(
        callers_of_helper.contains(&"chain_caller"),
        "reverse-deps('helper') doit trouver 'chain_caller'. trouve={callers_of_helper:?}"
    );
    assert!(
        callers_of_helper.contains(&"Parser::parse"),
        "reverse-deps('helper') doit trouver 'Parser::parse'. trouve={callers_of_helper:?}"
    );
}

// ── v0.6.4 : callgraph deps — path qualifiés (Axe C hybride) ────────────────
//
// Problème : `Token::new()` → seul `"new"` était stocké dans deps.
// Fix Axe C : stocker AUSSI le path qualifié `"Token::new"` pour permettre
// à code_scope_reverse_deps_batch (qui passe le qualified_name exact) de matcher.
// Compromis documenté : `self.set_pending()` → seul le terminal `"set_pending"` est
// stocké (le type du receiver n'est pas résolvable syntaxiquement).

/// Snippet représentatif : `caller_of_token_new` appelle `Token::new()` (scoped call_expression).
/// Le fix doit stocker BOTH `"new"` AND `"Token::new"` dans les deps du caller.
const SNIPPET_QUALIFIED_CALL: &str = r#"
pub struct Token {
    pub value: String,
}

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Token { value: value.into() }
    }
}

pub fn caller_of_token_new() -> Token {
    Token::new("hello")
}
"#;

/// `qualified_call_stores_both_terminal_and_path` :
/// `caller_of_token_new` appelle `Token::new(...)` →
/// ses deps doivent contenir `"new"` (terminal) ET `"Token::new"` (path qualifié).
///
/// Avant le fix Axe C, seul `"new"` était stocké → `reverse_deps("Token::new")` échouait.
#[test]
fn qualified_call_stores_both_terminal_and_path() {
    let symbols = parse_rust_file("src/qcall.rs", SNIPPET_QUALIFIED_CALL, true).expect("parse ok");

    let caller = symbols
        .iter()
        .find(|s| s.qualified_name == "caller_of_token_new")
        .expect("fn caller_of_token_new attendue");

    // Terminal — toujours présent (comportement pré-existant).
    assert!(
        caller.deps.contains(&"new".to_string()),
        "caller_of_token_new doit contenir le terminal 'new'. deps={:?}",
        caller.deps
    );

    // Path qualifié — nouveau comportement post-fix Axe C.
    assert!(
        caller.deps.contains(&"Token::new".to_string()),
        "caller_of_token_new doit contenir le path qualifié 'Token::new'. deps={:?}",
        caller.deps
    );
}

/// `stdlib_prefix_still_filtered` :
/// `std::mem::take(x)` → ni `"take"` (terminal) ni `"std::mem::take"` (path) ne doivent
/// être stockés. Le filtrage stdlib est inchangé après le fix.
const SNIPPET_STDLIB_CALL: &str = r#"
pub fn uses_stdlib() -> Option<String> {
    let mut x: Option<String> = Some("hello".to_string());
    std::mem::take(&mut x)
}
"#;

#[test]
fn stdlib_prefix_still_filtered() {
    let symbols = parse_rust_file("src/stdlib.rs", SNIPPET_STDLIB_CALL, true).expect("parse ok");

    let func = symbols
        .iter()
        .find(|s| s.qualified_name == "uses_stdlib")
        .expect("fn uses_stdlib attendue");

    assert!(
        !func.deps.contains(&"take".to_string()),
        "le terminal 'take' de std::mem::take ne doit PAS être dans deps. deps={:?}",
        func.deps
    );
    assert!(
        !func.deps.contains(&"std::mem::take".to_string()),
        "le path 'std::mem::take' ne doit PAS être dans deps. deps={:?}",
        func.deps
    );
}

/// `free_function_caller_unchanged` :
/// `validate_code_vault_id()` (fn libre, pas de `::` dans l'appel) →
/// seul le terminal est stocké. Comportement non-régressif.
const SNIPPET_FREE_FN_CALL: &str = r#"
pub fn validate_code_vault_id(s: &str) -> bool {
    s.starts_with("code-")
}

pub fn caller_of_free_fn() -> bool {
    validate_code_vault_id("code-test")
}
"#;

#[test]
fn free_function_caller_unchanged() {
    let symbols = parse_rust_file("src/free_fn.rs", SNIPPET_FREE_FN_CALL, true).expect("parse ok");

    let caller = symbols
        .iter()
        .find(|s| s.qualified_name == "caller_of_free_fn")
        .expect("fn caller_of_free_fn attendue");

    assert!(
        caller.deps.contains(&"validate_code_vault_id".to_string()),
        "caller_of_free_fn doit lister 'validate_code_vault_id'. deps={:?}",
        caller.deps
    );
}

// ── F-70 (v0.8.0) : résolution qualifiée des appels de méthode ──────────────
//
// Contexte : `code_scope_reverse_deps` cherche par `qualified_name` exact
// (`Type::method`). Avant F-70, `self.method()` ne stockait que le terminal
// (`"method"`), aveugle à `reverse_deps("Type::method")`.
//
// F-70 tâche 4.1 : le type de `self` EST connu syntaxiquement — c'est le type du
// bloc impl courant (déjà calculé par le parser). `self.parse()` dans `impl Parser`
// résout donc `Parser::parse` (terminal `parse` conservé EN PLUS).
// F-70 tâche 4.3 (précision > rappel) : les receivers non résolvables
// syntaxiquement (chaînage, inférence) retombent sur le terminal seul — jamais
// de faux positif dans le graphe inverse.
//
// NOTE : ce bloc de tests remplace l'ancien invariant P2-1 « self n'invente jamais
// de qualified_name », délibérément superseded par 4.1 : le type de self n'est PAS
// inventé, il est lu du bloc impl.

/// Snippet : appels de méthode via `field_expression` (grammaire 0.24.2).
///
/// - `self.parse()` : receiver `self` → terminal `"parse"` ET qualifié `"Parser::parse"` (F-70).
/// - `process("x")` : appel de fonction libre → terminal `"process"`.
const SNIPPET_METHOD_CALL_LIMITS: &str = r#"
pub struct Parser {
    input: String,
}

pub fn process(s: &str) -> usize {
    s.len()
}

impl Parser {
    pub fn run(&self) -> usize {
        let x = process("x");
        self.parse() + x
    }

    pub fn parse(&self) -> usize {
        process(&self.input)
    }
}
"#;

/// `method_call_terminal_is_emitted` :
/// `self.parse()` → deps de `Parser::run` doit contenir `"parse"` (terminal).
///
/// Vérifie que le terminal est bien extrait pour les `method_call_expression`.
#[test]
fn method_call_terminal_is_emitted() {
    let symbols = parse_rust_file("src/method_limits.rs", SNIPPET_METHOD_CALL_LIMITS, true)
        .expect("parse ok");

    // Toutes les méthodes extraites pour debug.
    let all_methods: Vec<(&str, &str)> = symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.qualified_name.as_str()))
        .collect();

    let run = symbols
        .iter()
        .find(|s| s.kind == "method" && s.qualified_name == "Parser::run")
        .unwrap_or_else(|| {
            panic!(
                "Parser::run doit être extrait en mode all=true. Symboles trouvés : {all_methods:?}"
            )
        });

    assert!(
        run.deps.contains(&"parse".to_string()),
        "Parser::run doit contenir le terminal 'parse' (self.parse()). deps={:?}",
        run.deps
    );
}

/// `method_call_self_receiver_resolves_qualified` (F-70 tâche 4.1) :
/// `self.parse()` dans `impl Parser` DOIT produire `"Parser::parse"` — le type de
/// `self` est celui du bloc impl courant (résolution syntaxique, pas d'invention).
///
/// Remplace l'ancien invariant P2-1 (« self n'invente jamais de qualified_name »),
/// délibérément superseded : le terminal `"parse"` reste présent EN PLUS.
#[test]
fn method_call_self_receiver_resolves_qualified() {
    let symbols = parse_rust_file("src/method_limits.rs", SNIPPET_METHOD_CALL_LIMITS, true)
        .expect("parse ok");

    let run = symbols
        .iter()
        .find(|s| s.kind == "method" && s.qualified_name == "Parser::run")
        .expect("Parser::run doit être extrait");

    assert!(
        run.deps.contains(&"Parser::parse".to_string()),
        "self.parse() dans impl Parser DOIT produire 'Parser::parse' (F-70 4.1). deps={:?}",
        run.deps
    );
    assert!(
        run.deps.contains(&"parse".to_string()),
        "le terminal 'parse' doit rester présent EN PLUS du qualifié. deps={:?}",
        run.deps
    );
}

// ── F-70 (v0.8.0) : résolution qualifiée self + bindings typés ──────────────
//
// Tâche 4.1 : `self.method()` → `Type::method` (type du bloc impl).
// Tâche 4.2 : bindings locaux typés (`let x: T`, params typés) → `T::method`.
// Tâche 4.3 : précision > rappel — receiver non résolvable → terminal seul.

/// Snippet couvrant self, binding `let` typé, param typé, inférence, chaînage.
const SNIPPET_F70_QUALIFIED: &str = r#"
pub struct SqliteIndex {
    n: u32,
}

fn make() -> SqliteIndex {
    SqliteIndex { n: 0 }
}

impl SqliteIndex {
    /// self.upsert() → SqliteIndex::upsert (tâche 4.1).
    pub fn run(&self) -> u32 {
        self.upsert()
    }

    /// self.clone() → filtré (STDLIB_NOISE_METHODS) : ni terminal ni qualifié.
    pub fn dup(&self) -> SqliteIndex {
        self.clone()
    }

    pub fn upsert(&self) -> u32 {
        1
    }

    pub fn begin(&self) -> SqliteIndex {
        make()
    }
}

/// let s: SqliteIndex → s.upsert() = SqliteIndex::upsert (tâche 4.2).
pub fn binding_caller() -> u32 {
    let s: SqliteIndex = make();
    s.upsert()
}

/// param typé store: SqliteIndex → store.upsert() = SqliteIndex::upsert (tâche 4.2).
pub fn param_caller(store: SqliteIndex) -> u32 {
    store.upsert()
}

/// let s = make() (inférence) → s.upsert() = terminal seul, PAS de qualifié (tâche 4.3).
pub fn infer_caller() -> u32 {
    let s = make();
    s.upsert()
}

/// chaînage store.begin().upsert() : begin résolu (receiver = store typé),
/// upsert NON résolu (receiver = call_expression) → terminal seul (tâche 4.3).
pub fn chain_caller(store: SqliteIndex) -> u32 {
    store.begin().upsert()
}
"#;

/// Helper : récupère les deps d'un symbole par qualified_name.
fn deps_of<'a>(symbols: &'a [gradatum_ingest::DerivedSymbol], qname: &str) -> &'a [String] {
    symbols
        .iter()
        .find(|s| s.qualified_name == qname)
        .unwrap_or_else(|| {
            let all: Vec<&str> = symbols.iter().map(|s| s.qualified_name.as_str()).collect();
            panic!("symbole '{qname}' absent. présents : {all:?}");
        })
        .deps
        .as_slice()
}

/// Tâche 4.1 : `self.upsert()` → deps contient `"upsert"` ET `"SqliteIndex::upsert"`.
#[test]
fn f70_self_call_resolved_to_impl_type() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "SqliteIndex::run");
    assert!(
        deps.contains(&"upsert".to_string()),
        "terminal 'upsert' attendu. deps={deps:?}"
    );
    assert!(
        deps.contains(&"SqliteIndex::upsert".to_string()),
        "qualifié 'SqliteIndex::upsert' attendu (self résolu). deps={deps:?}"
    );
}

/// Tâche 4.2 : `let s: SqliteIndex; s.upsert()` → `"SqliteIndex::upsert"`.
#[test]
fn f70_typed_let_binding_resolved() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "binding_caller");
    assert!(
        deps.contains(&"upsert".to_string()),
        "terminal 'upsert' attendu. deps={deps:?}"
    );
    assert!(
        deps.contains(&"SqliteIndex::upsert".to_string()),
        "qualifié 'SqliteIndex::upsert' attendu (binding typé). deps={deps:?}"
    );
}

/// Tâche 4.2 : param typé `store: SqliteIndex` → `store.upsert()` = `"SqliteIndex::upsert"`.
#[test]
fn f70_typed_param_resolved() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "param_caller");
    assert!(
        deps.contains(&"SqliteIndex::upsert".to_string()),
        "qualifié 'SqliteIndex::upsert' attendu (param typé). deps={deps:?}"
    );
}

/// Tâche 4.3 : `let s = make()` (inférence) → terminal seul, ZÉRO faux positif.
#[test]
fn f70_inferred_binding_not_resolved() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "infer_caller");
    assert!(
        deps.contains(&"upsert".to_string()),
        "terminal 'upsert' attendu. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"SqliteIndex::upsert".to_string()),
        "type inféré non explicite → PAS de qualifié (précision > rappel). deps={deps:?}"
    );
}

/// Tâche 4.3 : chaînage `store.begin().upsert()` — receiver de `upsert` = call_expression
/// (non un identifier simple) → terminal seul, PAS de qualifié (zéro faux positif).
#[test]
fn f70_chained_receiver_not_resolved() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "chain_caller");
    // upsert : receiver = call_expression (store.begin()) → non résolu, terminal seul.
    assert!(
        deps.contains(&"upsert".to_string()),
        "terminal 'upsert' attendu. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"SqliteIndex::upsert".to_string()),
        "receiver chaîné (call_expression) non résolvable → PAS de qualifié. deps={deps:?}"
    );
}

/// Tâche 4.3 / filtre stdlib : `self.clone()` → ni `"clone"` ni `"SqliteIndex::clone"`.
#[test]
fn f70_self_stdlib_method_still_filtered() {
    let symbols = parse_rust_file("src/idx.rs", SNIPPET_F70_QUALIFIED, true).expect("parse ok");
    let deps = deps_of(&symbols, "SqliteIndex::dup");
    assert!(
        !deps.contains(&"clone".to_string()),
        "terminal stdlib 'clone' doit rester filtré. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"SqliteIndex::clone".to_string()),
        "self.clone() ne doit PAS produire 'SqliteIndex::clone' (filtre stdlib). deps={deps:?}"
    );
}

/// Non-régression : `A::b()` (scoped_identifier) stocke toujours les DEUX formes.
#[test]
fn f70_scoped_call_still_stores_both_forms() {
    const S: &str = r#"
pub struct Token;
impl Token {
    pub fn new() -> Token { Token }
}
pub fn caller() -> Token {
    Token::new()
}
"#;
    let symbols = parse_rust_file("src/tok.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "caller");
    assert!(
        deps.contains(&"new".to_string()),
        "terminal 'new' attendu (non-régression Axe C). deps={deps:?}"
    );
    assert!(
        deps.contains(&"Token::new".to_string()),
        "qualifié 'Token::new' attendu (non-régression Axe C). deps={deps:?}"
    );
}

// ── F-70 audit post-mortem : fixes precision-first C1 + R1 ──────────────────

/// C1 (P1) : rebind explicite→inféré du même nom. `fn transform(x: Foo){ let x = make_bar();
/// x.run() }` — le type réel de `x` au point d'appel est `Bar`, PAS `Foo`. Un binding inféré
/// du même nom doit RETIRER le type précédent (rebind = drop). Assert : ABSENCE de `Foo::run`.
#[test]
fn f70_c1_reinferred_shadow_drops_typed() {
    const S: &str = r#"
pub struct Foo;
pub struct Bar;
impl Foo { pub fn run(&self) -> u32 { 0 } }
impl Bar { pub fn run(&self) -> u32 { 1 } }
fn make_bar() -> Bar { Bar }
pub fn transform(x: Foo) -> u32 {
    let x = make_bar();
    x.run()
}
"#;
    let symbols = parse_rust_file("src/shadow.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "transform");
    assert!(
        deps.contains(&"run".to_string()),
        "terminal 'run' attendu. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"Foo::run".to_string()),
        "rebind inféré `let x = make_bar()` masque le param `x: Foo` → PAS de 'Foo::run' \
         (faux positif prouve par la revue de code). deps={deps:?}"
    );
    assert!(
        !deps.contains(&"Bar::run".to_string()),
        "type du rebind non explicite (inféré) → PAS de 'Bar::run' non plus. deps={deps:?}"
    );
}

/// R1 (P2) : types conteneurs/wrappers stdlib exclus de la qualification.
/// `let v: Vec<u32>; v.sort_by(..)` → pas de `Vec::sort_by` (bruit).
#[test]
fn f70_r1_container_type_not_qualified() {
    const S: &str = r#"
fn make() -> Vec<u32> { vec![] }
pub fn sorter() -> u32 {
    let mut v: Vec<u32> = make();
    v.sort_by(|a, b| a.cmp(b));
    0
}
"#;
    let symbols = parse_rust_file("src/cont.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "sorter");
    assert!(
        !deps.contains(&"Vec::sort_by".to_string()),
        "type conteneur stdlib Vec → PAS de 'Vec::sort_by'. deps={deps:?}"
    );
}

/// C1-bis (re-audit, prouvé) : shadowing par destructuring composite = faux positif.
/// `fn destr(x: Foo){ let (x, _y) = pair(); x.run() }` — le type réel de `x` est le
/// résultat du destructuring (u32), PAS Foo. Le rebind par tuple_pattern doit poisonner
/// `x` (≥2 sites → drop). Assert : ABSENCE de `Foo::run`.
#[test]
fn f70_c1bis_destructuring_shadow_drops_typed() {
    const S: &str = r#"
pub struct Foo;
impl Foo { pub fn run(&self) -> u32 { 0 } }
fn pair() -> (u32, u32) { (1, 2) }
pub fn destr(x: Foo) -> u32 {
    let (x, _y) = pair();
    x.run()
}
"#;
    let symbols = parse_rust_file("src/destr.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "destr");
    assert!(
        deps.contains(&"run".to_string()),
        "terminal 'run' attendu. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"Foo::run".to_string()),
        "rebind par destructuring `let (x, _y) = pair()` masque `x: Foo` → PAS de 'Foo::run' \
         (faux positif prouvé au re-audit). deps={deps:?}"
    );
}

/// Non-régression : un paramètre destructuré `fn f((a, _): (Foo, u32))` reste NON résolu
/// (comportement safe actuel — `a.run()` ne doit pas produire `Foo::run`).
#[test]
fn f70_c1bis_destructured_param_stays_unresolved() {
    const S: &str = r#"
pub struct Foo;
impl Foo { pub fn run(&self) -> u32 { 0 } }
pub fn param_destr((a, _): (Foo, u32)) -> u32 {
    a.run()
}
"#;
    let symbols = parse_rust_file("src/pdestr.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "param_destr");
    assert!(
        !deps.contains(&"Foo::run".to_string()),
        "param destructuré → type par élément non attribué → PAS de 'Foo::run'. deps={deps:?}"
    );
}

/// R1 (P2) : wrapper `Arc<Foo>` — mis-qualification Deref évitée.
/// `let x: Arc<Foo>; x.run()` (méthode de Foo via Deref) → PAS de `Arc::run` (faux), terminal seul.
#[test]
fn f70_r1_deref_wrapper_not_misqualified() {
    const S: &str = r#"
use std::sync::Arc;
pub struct Foo;
impl Foo { pub fn run(&self) -> u32 { 0 } }
pub fn arc_caller(x: Arc<Foo>) -> u32 {
    x.run()
}
"#;
    let symbols = parse_rust_file("src/wrap.rs", S, true).expect("parse ok");
    let deps = deps_of(&symbols, "arc_caller");
    assert!(
        deps.contains(&"run".to_string()),
        "terminal 'run' attendu. deps={deps:?}"
    );
    assert!(
        !deps.contains(&"Arc::run".to_string()),
        "wrapper Arc → PAS de 'Arc::run' (mis-qualification Deref évitée). deps={deps:?}"
    );
}

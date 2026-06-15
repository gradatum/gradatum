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
/// `str::floor_char_boundary(120)` (stable Rust ≥ 1.93) ajuste l'offset au
/// dernier codepoint complet ≤ 120 bytes.
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

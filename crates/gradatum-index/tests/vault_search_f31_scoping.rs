//! Tests F-31 — scoping optionnel `locus` + `vault_id` dans `search_fts_with_snippet`
//! et `search_semantic`.
//!
//! Couvre :
//! 1. `locus_filter_fts_restricts_by_prefix` — filtre locus FTS : ne retourne que les
//!    notes dont le locus commence par le préfixe.
//! 2. `locus_none_returns_all_notes` — sans filtre locus : comportement inchangé
//!    (non-régression).
//! 3. `locus_percent_literal_no_match` — anti-injection LIKE : `locus="%"` ne matche
//!    aucune note hors préfixe littéral `%` (test négatif obligatoire).
//! 4. `locus_underscore_literal` — métacaractère `_` échappé : ne matche pas un
//!    caractère unique arbitraire.
//! 5. `locus_backslash_literal` — métacaractère `\` échappé.
//! 6. `vault_id_scoping_fts` — vault_id ≠ main : seules les notes du vault demandé
//!    sont retournées (isolation cross-vault FTS).
//! 7. `locus_filter_semantic` — filtre locus chemin sémantique.
//! 8. `escape_like_roundtrip` — vérification unitaire de la fonction `escape_like`.

// VectorStore : nécessaire pour résoudre insert_note_embedding/search_semantic sur SqliteIndex.
use gradatum_core::scope::VaultId;
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

// ── Tests FTS ─────────────────────────────────────────────────────────────────
// Note : escape_like est testé dans gradatum-dto (unit test + doc-test).
// Les tests ci-dessous couvrent le comportement observable de l'échappement
// via les résultats des requêtes FTS réelles.

/// Test 1 : filtre locus FTS — préfixe `council/` restreint aux notes de council.
#[tokio::test]
async fn locus_filter_fts_restricts_by_prefix() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_council = ulid::Ulid::new().to_string();
    let id_other = ulid::Ulid::new().to_string();

    // Note dans locus council/
    idx.seed_note_with_fts_vault(
        &id_council,
        "main",
        "council",
        Some("council/art19/2026-06"),
        "scoping locus council decision gradatum",
    )
    .await
    .expect("seed council");

    // Note dans locus decisions/ (hors filtre)
    idx.seed_note_with_fts_vault(
        &id_other,
        "main",
        "decisions",
        Some("decisions/2026-06"),
        "scoping locus decisions gradatum",
    )
    .await
    .expect("seed decisions");

    // Filtre locus = "council/" → seule la note council
    let results = idx
        .search_fts_with_snippet(&vault, "scoping", 10, false, None, Some("council/"), None)
        .await
        .expect("search FTS avec locus council/");

    assert_eq!(
        results.len(),
        1,
        "filtre locus council/ → 1 résultat attendu, got {}",
        results.len()
    );
    assert_eq!(
        results[0].note_id.to_string(),
        id_council,
        "doit retourner la note council"
    );
}

/// Test 2 : sans filtre locus — comportement inchangé (non-régression).
#[tokio::test]
async fn locus_none_returns_all_notes() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_a = ulid::Ulid::new().to_string();
    let id_b = ulid::Ulid::new().to_string();

    idx.seed_note_with_fts_vault(
        &id_a,
        "main",
        "council",
        Some("council/a"),
        "nonregression locus filtre absent",
    )
    .await
    .expect("seed a");

    idx.seed_note_with_fts_vault(
        &id_b,
        "main",
        "decisions",
        Some("decisions/b"),
        "nonregression locus filtre absent",
    )
    .await
    .expect("seed b");

    // Pas de filtre locus → les deux notes
    let results = idx
        .search_fts_with_snippet(&vault, "nonregression", 10, false, None, None, None)
        .await
        .expect("search FTS sans locus");

    assert!(
        results.len() >= 2,
        "sans filtre locus → ≥2 résultats attendus, got {}",
        results.len()
    );
    let ids: Vec<String> = results.iter().map(|h| h.note_id.to_string()).collect();
    assert!(ids.contains(&id_a), "id_a doit être présent");
    assert!(ids.contains(&id_b), "id_b doit être présent");
}

/// Test 3 : anti-injection LIKE — `locus="%"` ne matche aucune note dont le locus
/// ne commence pas littéralement par `%`.
///
/// Garantit que les métacaractères LIKE sont bien échappés (escape_like).
#[tokio::test]
async fn locus_percent_literal_no_match() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_normal = ulid::Ulid::new().to_string();

    // Note avec locus normal (sans `%`)
    idx.seed_note_with_fts_vault(
        &id_normal,
        "main",
        "decisions",
        Some("decisions/normal"),
        "test injection percent wildcard",
    )
    .await
    .expect("seed normal");

    // Filtre locus = "%" — sans échappement serait un joker universel.
    // Avec escape_like : traité littéralement → ne matche QUE les locus commençant par "%".
    let results = idx
        .search_fts_with_snippet(&vault, "test", 10, false, None, Some("%"), None)
        .await
        .expect("search avec locus=%");

    // La note normale n'a pas un locus commençant par "%" → 0 résultats.
    assert_eq!(
        results.len(),
        0,
        "locus='%' ne doit PAS matcher une note avec locus='decisions/normal' — \
         anti-injection LIKE : % doit être traité littéralement. got {}",
        results.len()
    );
}

/// Test 4 : métacaractère `_` échappé — ne matche pas n'importe quel caractère.
#[tokio::test]
async fn locus_underscore_literal() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_a = ulid::Ulid::new().to_string();
    let id_b = ulid::Ulid::new().to_string();

    // Note avec locus "aXb" (X = caractère quelconque)
    idx.seed_note_with_fts_vault(
        &id_a,
        "main",
        "decisions",
        Some("aXb/note"),
        "underscore locus escape test keyword",
    )
    .await
    .expect("seed aXb");

    // Note avec locus "a_b" (underscore littéral)
    idx.seed_note_with_fts_vault(
        &id_b,
        "main",
        "decisions",
        Some("a_b/note"),
        "underscore locus escape test keyword",
    )
    .await
    .expect("seed a_b");

    // Filtre "a_b" → avec échappement, matche UNIQUEMENT le locus "a_b/..." (underscore littéral).
    let results = idx
        .search_fts_with_snippet(&vault, "underscore", 10, false, None, Some("a_b"), None)
        .await
        .expect("search avec locus a_b");

    let ids: Vec<String> = results.iter().map(|h| h.note_id.to_string()).collect();
    assert!(
        ids.contains(&id_b),
        "la note avec locus 'a_b/note' doit être retournée"
    );
    assert!(
        !ids.contains(&id_a),
        "la note avec locus 'aXb/note' NE doit PAS être retournée — \
         '_' doit être traité littéralement, pas comme wildcard"
    );
}

/// Test 5 : métacaractère `\` échappé.
#[tokio::test]
async fn locus_backslash_literal() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_slash = ulid::Ulid::new().to_string();
    let id_backslash = ulid::Ulid::new().to_string();

    idx.seed_note_with_fts_vault(
        &id_slash,
        "main",
        "decisions",
        Some("path/normal"),
        "backslash locus escape test",
    )
    .await
    .expect("seed slash");

    // Locus avec backslash littéral
    idx.seed_note_with_fts_vault(
        &id_backslash,
        "main",
        "decisions",
        Some("path\\special"),
        "backslash locus escape test",
    )
    .await
    .expect("seed backslash");

    // Filtre "path\\special" → doit matcher uniquement le locus avec backslash
    let results = idx
        .search_fts_with_snippet(
            &vault,
            "backslash",
            10,
            false,
            None,
            Some("path\\special"),
            None,
        )
        .await
        .expect("search avec locus backslash");

    let ids: Vec<String> = results.iter().map(|h| h.note_id.to_string()).collect();
    assert!(
        ids.contains(&id_backslash),
        "la note avec locus 'path\\special' doit être retournée"
    );
    assert!(
        !ids.contains(&id_slash),
        "la note avec locus 'path/normal' NE doit PAS être retournée"
    );
}

/// Test 6 : vault_id ≠ main — isolation cross-vault FTS.
///
/// Une requête sur vault_id="secondary" ne retourne que les notes de ce vault.
#[tokio::test]
async fn vault_id_scoping_fts() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let id_main = ulid::Ulid::new().to_string();
    let id_secondary = ulid::Ulid::new().to_string();

    // Note dans vault "main"
    idx.seed_note_with_fts_vault(
        &id_main,
        "main",
        "decisions",
        None,
        "vault scoping cross-tenant isolation test",
    )
    .await
    .expect("seed main");

    // Note dans vault "secondary"
    idx.seed_note_with_fts_vault(
        &id_secondary,
        "secondary",
        "decisions",
        None,
        "vault scoping cross-tenant isolation test",
    )
    .await
    .expect("seed secondary");

    // Requête sur vault "secondary" → uniquement la note secondary
    let vault_sec = VaultId::new("secondary");
    let results = idx
        .search_fts_with_snippet(&vault_sec, "vault", 10, false, None, None, None)
        .await
        .expect("search vault secondary");

    let ids: Vec<String> = results.iter().map(|h| h.note_id.to_string()).collect();
    assert!(
        ids.contains(&id_secondary),
        "la note 'secondary' doit être retournée"
    );
    assert!(
        !ids.contains(&id_main),
        "la note 'main' NE doit PAS être retournée quand vault='secondary'"
    );
}

/// Test 7 : filtre locus chemin sémantique — seules les notes avec locus conseil
/// sont chargées pour le cosine.
#[tokio::test]
async fn locus_filter_semantic() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let id_council_ulid = ulid::Ulid::new();
    let id_other_ulid = ulid::Ulid::new();
    let id_council = id_council_ulid.to_string();
    let id_other = id_other_ulid.to_string();

    // Note "council" avec embedding aligné sur la query
    let emb_council = vec![1.0f32, 0.0, 0.0, 0.0];
    idx.seed_note_with_fts_vault(
        &id_council,
        "main",
        "council",
        Some("council/arc19"),
        "locus semantic filter gradatum",
    )
    .await
    .expect("seed council");
    idx.insert_note_embedding(
        &gradatum_core::identity::NoteId(id_council_ulid),
        "test-sem-locus",
        4,
        &emb_council,
    )
    .await
    .expect("insert embedding council");

    // Note "decisions" avec embedding identique
    idx.seed_note_with_fts_vault(
        &id_other,
        "main",
        "decisions",
        Some("decisions/2026"),
        "locus semantic filter gradatum",
    )
    .await
    .expect("seed decisions");
    idx.insert_note_embedding(
        &gradatum_core::identity::NoteId(id_other_ulid),
        "test-sem-locus",
        4,
        &emb_council,
    )
    .await
    .expect("insert embedding decisions");

    // Requête sémantique avec locus "council/"
    let query_emb = vec![1.0f32, 0.0, 0.0, 0.0];
    let hits = idx
        .search_semantic("main", "test-sem-locus", &query_emb, 10, Some("council/"))
        .await
        .expect("search_semantic avec locus");

    let ids: Vec<String> = hits.iter().map(|(id, _)| id.to_string()).collect();
    assert!(
        ids.contains(&id_council),
        "la note council doit être retournée"
    );
    assert!(
        !ids.contains(&id_other),
        "la note decisions NE doit PAS être retournée avec filtre locus=council/"
    );
}

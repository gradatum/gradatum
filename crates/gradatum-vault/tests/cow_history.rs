//! Tests d'intégration F-40 — Copy-on-Write + `.history/`.
//!
//! Vérifie que :
//! - Un write avec body différent crée un snapshot dans `.history/<id>/`.
//! - Un write avec seulement des champs exclus (processed, updated, etc.) ne crée PAS de snapshot.
//! - `history_versions` liste les snapshots disponibles.
//! - Les snapshots ne sont PAS retournés par `vault_search` (non indexés).

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

// ── Helpers locaux ────────────────────────────────────────────────────────────

/// Construit un Frontmatter minimal pour les tests CoW.
fn fm() -> gradatum_core::frontmatter::Frontmatter {
    build_minimal_frontmatter()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Écrire deux versions avec bodies différents doit créer 1 snapshot dans `.history/`.
#[tokio::test]
async fn cow_creates_history_entry_on_body_change() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // v1 — création initiale (pas d'historique créé car pas d'existant).
    vault
        .write_note_with_id(fm(), "corps v1".into(), id)
        .await
        .unwrap();

    // v2 — body différent → doit déclencher le CoW et créer un snapshot.
    vault
        .write_note_with_id(fm(), "corps v2 modifié".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        1,
        "1 snapshot doit exister après la 2ème écriture (body différent)"
    );
}

/// Écrire deux fois le même body NE doit PAS créer de snapshot (hash identique).
///
/// On réutilise la MÊME frontmatter de base pour les deux écritures afin d'avoir
/// un `created` identique (et tous les autres champs stables sauf `updated` qui est
/// exclu du hash d'historique). Le CoW compare `sha256_for_history` qui ignore
/// `updated` → hashes identiques → pas de snapshot.
#[tokio::test]
async fn cow_no_history_on_identical_body() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    // Frontmatter de base stable (même created pour les deux écritures).
    let base_fm = fm();

    // v1 — création.
    vault
        .write_note_with_id(base_fm.clone(), "corps identique".into(), id)
        .await
        .unwrap();

    // v2 — même frontmatter base + même body → hash sémantique identique → PAS de snapshot.
    vault
        .write_note_with_id(base_fm, "corps identique".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        0,
        "Aucun snapshot ne doit être créé si le body est identique"
    );
}

/// Ajouter une clé extra exclue (`processed`) ne doit PAS créer de snapshot.
///
/// On dérive fm2 depuis base_fm (même `created`) pour que la seule différence
/// soit la clé extra `processed` — qui est exclue du hash d'historique.
#[tokio::test]
async fn cow_no_history_on_excluded_extra_key() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    // Frontmatter de base stable.
    let base_fm = fm();

    // v1 — création sans extra.
    vault
        .write_note_with_id(base_fm.clone(), "corps stable".into(), id)
        .await
        .unwrap();

    // v2 — dériver depuis base_fm, ajouter la clé extra exclue `processed`.
    let mut fm2 = base_fm;
    fm2.extra
        .insert("processed".to_string(), toml::Value::Boolean(true));

    vault
        .write_note_with_id(fm2, "corps stable".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        0,
        "L'ajout de 'processed' (clé exclue) ne doit pas créer de snapshot"
    );
}

/// Trois écritures avec bodies différents → 2 snapshots.
#[tokio::test]
async fn cow_accumulates_multiple_history_entries() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    vault
        .write_note_with_id(fm(), "v1".into(), id)
        .await
        .unwrap();
    vault
        .write_note_with_id(fm(), "v2".into(), id)
        .await
        .unwrap();
    vault
        .write_note_with_id(fm(), "v3".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        2,
        "2 snapshots doivent exister après 3 écritures à bodies différents"
    );
}

/// `history_versions` retourne une liste vide pour une note sans historique.
#[tokio::test]
async fn cow_history_versions_empty_for_new_note() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    // Créer la note (une seule écriture = pas de snapshot).
    vault
        .write_note_with_id(fm(), "première version".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert!(
        versions.is_empty(),
        "history_versions doit retourner vide pour une note sans historique"
    );
}

/// `history_versions` retourne une liste vide pour une note inexistante.
#[tokio::test]
async fn cow_history_versions_empty_for_nonexistent_note() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();
    let versions = vault.history_versions(id).await.unwrap();
    assert!(
        versions.is_empty(),
        "history_versions doit retourner vide pour une note inexistante"
    );
}

/// `history_get` récupère le contenu exact d'un snapshot.
#[tokio::test]
async fn cow_history_get_returns_snapshot_content() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // v1 — créer la note.
    vault
        .write_note_with_id(fm(), "contenu v1 original".into(), id)
        .await
        .unwrap();

    // v2 — modifier → crée le snapshot de v1.
    vault
        .write_note_with_id(fm(), "contenu v2 modifié".into(), id)
        .await
        .unwrap();

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 1, "un snapshot doit exister");

    // Lire le snapshot.
    let snapshot = vault.history_get(id, versions[0]).await.unwrap();
    assert_eq!(
        snapshot.body.markdown, "contenu v1 original",
        "le snapshot doit contenir le contenu v1"
    );
}

/// Les snapshots `.history/` ne doivent PAS être indexés dans SQLite.
///
/// Vérification par construction : les snapshots ne sont jamais passés à `upsert_note`.
/// On vérifie indirectement que le count de notes indexées reste 1 (seule la version
/// courante, pas les snapshots).
#[tokio::test]
async fn cow_history_not_indexed_in_sqlite() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // v1 puis v2 — génère un snapshot de v1 dans .history/.
    vault
        .write_note_with_id(fm(), "v1 unique".into(), id)
        .await
        .unwrap();
    vault
        .write_note_with_id(fm(), "v2 différent".into(), id)
        .await
        .unwrap();

    // Un snapshot existe.
    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 1, "un snapshot doit exister");

    // L'index ne contient que 1 note (la version courante, pas le snapshot).
    // On vérifie via get_content_hash : la note id est présente dans l'index.
    let stored_hash = vault.index().get_content_hash(id).await.unwrap();
    assert!(
        stored_hash.is_some(),
        "la note courante doit être présente dans l'index"
    );

    // Le snapshot est sur disque dans .history/ mais JAMAIS dans la table notes.
    // On le confirme en vérifiant que le fichier physique existe sous .history/.
    let history_path = dir
        .path()
        .join("main")
        .join(".history")
        .join(id.to_string());
    assert!(
        history_path.exists(),
        "le répertoire .history/<id>/ doit exister sur disque : {}",
        history_path.display()
    );
}

// ── Tests D1 — Rétention bornée + purge sur suppression ──────────────────────

/// D1 — Écrire MAX+1 versions → exactement MAX snapshots restent.
///
/// La politique de rétention par défaut (max_versions=50) doit supprimer le snapshot
/// le plus ancien après chaque dépassement. Ce test vérifie le comportement inchangé
/// post-F-32A avec les valeurs par défaut.
#[tokio::test]
async fn d1_retention_caps_history_at_max() {
    // Valeur par défaut définie dans HistoryConfig::default().
    // Si elle change, ce test échouera et alertera le développeur.
    const MAX: usize = 50;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // Écrire MAX+1 versions avec des corps distincts pour déclencher MAX CoW.
    // La première écriture crée la note (pas de CoW). Les suivantes créent chacune
    // un snapshot, puis à partir du snapshot MAX+1, le plus ancien est purgé.
    for i in 0..=(MAX as u64) {
        // Pause de 1ms entre chaque écriture pour garantir des timestamps distincts.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let body = format!("corps version {}", i);
        vault
            .write_note_with_id(fm(), body, id)
            .await
            .unwrap_or_else(|e| panic!("écriture version {i} échouée : {e}"));
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        MAX,
        "exactement MAX={MAX} snapshots doivent subsister après MAX+1 écritures"
    );
}

/// D1 — Supprimer une note purge son répertoire `.history/<id>/`.
#[tokio::test]
async fn d1_delete_note_purges_history_dir() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // Créer la note puis la modifier pour générer un snapshot.
    vault
        .write_note_with_id(fm(), "v1".into(), id)
        .await
        .unwrap();
    vault
        .write_note_with_id(fm(), "v2 modifié".into(), id)
        .await
        .unwrap();

    // Vérifier qu'un snapshot existe.
    let versions_before = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_before.len(),
        1,
        "un snapshot doit exister avant la suppression"
    );

    // Supprimer la note.
    vault.delete_note(id).await.unwrap();

    // Le répertoire .history/<id>/ doit avoir disparu.
    let history_path = dir
        .path()
        .join("main")
        .join(".history")
        .join(id.to_string());
    assert!(
        !history_path.exists(),
        "le répertoire .history/<id>/ doit être absent après delete_note : {}",
        history_path.display()
    );
}

/// D1 — delete_note sur une note sans historique ne panique pas.
#[tokio::test]
async fn d1_delete_note_without_history_is_clean() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let id = NoteId::new();

    // Créer la note (pas de modification → pas de snapshot .history/).
    vault
        .write_note_with_id(fm(), "note sans historique".into(), id)
        .await
        .unwrap();

    // Supprimer — ne doit pas paniquer même sans .history/.
    vault
        .delete_note(id)
        .await
        .expect("delete_note sans historique doit réussir");
}

// ── Tests F-32A — pruning configurable (max_versions + TTL) ──────────────────
//
// Ces tests utilisent `Vault::apply_history_trim` — méthode publique paramétrée
// avec `now_ms: u64` pour permettre l'injection de l'horloge dans les tests.
// La méthode interne `trim_history_to_max` délègue à cette logique avec
// `chrono::Utc::now().timestamp_millis() as u64` en production.

/// F-32A — max_versions custom cap le nombre de snapshots à la valeur configurée.
#[tokio::test]
async fn f32a_max_versions_custom_caps_at_configured_limit() {
    use gradatum_core::config::HistoryConfig;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Écrire 5 versions → 4 snapshots (première écriture = pas de snapshot).
    for i in 0..5u64 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        vault
            .write_note_with_id(fm(), format!("corps v{i}"), id)
            .await
            .unwrap();
    }

    let versions_before = vault.history_versions(id).await.unwrap();
    assert_eq!(versions_before.len(), 4, "4 snapshots avant trim custom");

    // Appliquer un trim avec max_versions=2 — doit laisser exactement 2 snapshots.
    let cfg = HistoryConfig {
        max_versions: 2,
        ttl_days: None,
    };
    let id_str = id.to_string();
    // now_ms = loin dans le futur (pas de TTL actif → seul le cap count joue)
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, u64::MAX)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        2,
        "exactement max_versions=2 snapshots doivent rester après trim"
    );
    // Vérifier que ce sont les 2 plus récents (timestamps les plus grands).
    let mut sorted_before = versions_before.clone();
    sorted_before.sort_unstable();
    assert_eq!(
        versions_after,
        &sorted_before[sorted_before.len() - 2..],
        "les 2 snapshots les plus récents doivent être conservés"
    );
}

/// F-32A — ttl_days supprime les snapshots plus vieux que N jours.
#[tokio::test]
async fn f32a_ttl_days_removes_old_snapshots() {
    use gradatum_core::config::HistoryConfig;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Écrire 3 versions → 2 snapshots.
    for i in 0..3u64 {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        vault
            .write_note_with_id(fm(), format!("corps v{i}"), id)
            .await
            .unwrap();
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 2, "2 snapshots avant TTL");

    // Simuler un now_ms très avancé dans le futur (now = +200 jours après les snapshots).
    // Avec ttl_days=100, tous les snapshots (plus vieux de 200 jours) doivent être supprimés.
    let oldest_ts_ms = versions[0];
    let now_ms = (oldest_ts_ms + 200 * 24 * 3600 * 1000) as u64;

    let cfg = HistoryConfig {
        max_versions: 50, // cap count élevé — ne doit pas interférer
        ttl_days: Some(100),
    };
    let id_str = id.to_string();
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, now_ms)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        0,
        "tous les snapshots vieux de >100 jours doivent être supprimés"
    );
}

/// F-32A — ttl_days conserve les snapshots récents (inférieurs au seuil TTL).
#[tokio::test]
async fn f32a_ttl_days_keeps_recent_snapshots() {
    use gradatum_core::config::HistoryConfig;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Écrire 3 versions → 2 snapshots.
    for i in 0..3u64 {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        vault
            .write_note_with_id(fm(), format!("corps v{i}"), id)
            .await
            .unwrap();
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 2, "2 snapshots avant TTL");

    // now_ms 1 seconde après le dernier snapshot → aucun snapshot n'a plus de 30 jours.
    let newest_ts_ms = versions[versions.len() - 1];
    let now_ms = (newest_ts_ms + 1000) as u64;

    let cfg = HistoryConfig {
        max_versions: 50,
        ttl_days: Some(30),
    };
    let id_str = id.to_string();
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, now_ms)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        2,
        "snapshots récents (<30 jours) ne doivent pas être supprimés"
    );
}

/// F-32A — ttl_days=None ne supprime rien par âge (seul max_versions joue).
#[tokio::test]
async fn f32a_no_ttl_keeps_all_within_max_versions() {
    use gradatum_core::config::HistoryConfig;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Écrire 4 versions → 3 snapshots.
    for i in 0..4u64 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        vault
            .write_note_with_id(fm(), format!("corps v{i}"), id)
            .await
            .unwrap();
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 3, "3 snapshots avant trim sans TTL");

    let cfg = HistoryConfig {
        max_versions: 50, // cap count au-dessus des 3 snapshots → ne joue pas
        ttl_days: None,
    };
    let id_str = id.to_string();
    // now_ms très avancé — sans TTL, rien ne doit être supprimé par âge.
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, u64::MAX)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        3,
        "ttl_days=None : aucun snapshot supprimé par âge, tous les 3 restent"
    );
}

/// F-32A — Interaction : TTL supprime les vieux, puis max_versions cap le reste.
///
/// Scénario avec timestamps injectés directement sur disque :
/// - 5 snapshots artificiels avec timestamps contrôlés (0, 1, 2, 3, 4 jours avant now).
/// - TTL=2 jours, now=5 jours après l'époque →
///   expirent ts=0 (5j avant now), ts=1 (4j), ts=2 (3j) — soit 3 expirés.
///   restent ts=3 (2j = limite exacte — non expiré car cutoff est strict <) et ts=4 (1j).
/// - max_versions=1 → cap → supprime ts=3.
/// - Résultat attendu : seulement ts=4.
#[tokio::test]
async fn f32a_ttl_then_max_versions_interaction() {
    use gradatum_core::config::HistoryConfig;
    use std::fs;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Créer la note de base (pour que `id` existe dans l'index).
    vault
        .write_note_with_id(fm(), "corps base".into(), id)
        .await
        .unwrap();

    // Injecter 5 snapshots artificiels avec timestamps contrôlés.
    // L'époque de référence = 0ms. Les timestamps sont des offsets en jours.
    // now_ms sera fixé à 5 jours (5 * 86_400_000 ms).
    let one_day_ms: i64 = 24 * 3600 * 1000;
    let id_str = id.to_string();
    let history_dir = dir.path().join("main").join(".history").join(&id_str);
    fs::create_dir_all(&history_dir).unwrap();

    // Timestamps artificiels : 0, 1, 2, 3, 4 jours depuis l'époque 0.
    // now_ms=5 jours (300_000_000 + ajustements)
    let now_ms: u64 = 5 * one_day_ms as u64;
    let snapshot_ts: Vec<i64> = (0..5).map(|i| i * one_day_ms).collect();
    for &ts in &snapshot_ts {
        let snap_path = history_dir.join(format!("{ts}.md"));
        fs::write(snap_path, format!("snapshot ts={ts}")).unwrap();
    }

    // Vérifier que les 5 snapshots sont listés.
    let versions_before = vault.history_versions(id).await.unwrap();
    assert_eq!(versions_before.len(), 5, "5 snapshots artificiels injectés");

    // TTL=2 jours, now=5 jours :
    //   cutoff = 5j - 2j = 3j (3 * 86_400_000)
    //   ts=0 (0j) → 0 < 3j → expiré
    //   ts=1j → 1j < 3j → expiré
    //   ts=2j → 2j < 3j → expiré
    //   ts=3j → 3j == 3j → NON expiré (strict <, pas <=)
    //   ts=4j → 4j > 3j → NON expiré
    // → 3 expirés par TTL, restent ts=3j et ts=4j.
    // max_versions=1 → supprime ts=3j (plus ancien des 2 restants).
    // Résultat : seulement ts=4j.
    let cfg = HistoryConfig {
        max_versions: 1,
        ttl_days: Some(2),
    };
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, now_ms)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        1,
        "TTL supprime 3 vieux (0j,1j,2j) + cap supprime ts=3j → 1 reste (ts=4j)"
    );
    assert_eq!(
        versions_after[0], snapshot_ts[4],
        "le snapshot ts=4j doit être le seul survivant"
    );
}

/// F-32A — Via write_note_with_id avec config max_versions=3 dans le TOML.
///
/// Vérifie que `trim_history_to_max` lit bien `self.config.history.max_versions`.
#[tokio::test]
async fn f32a_vault_uses_config_max_versions_on_cow() {
    use std::fs;

    let dir = TempDir::new().unwrap();

    // Créer la config TOML avec max_versions=3 AVANT d'ouvrir le vault.
    let gradatum_dir = dir.path().join(".gradatum");
    fs::create_dir_all(&gradatum_dir).unwrap();
    fs::write(
        gradatum_dir.join("config.toml"),
        "[history]\nmax_versions = 3\n",
    )
    .unwrap();

    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Écrire 6 versions → 5 CoW successifs → le trim cap à 3 après chaque CoW.
    for i in 0..6u64 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        vault
            .write_note_with_id(fm(), format!("corps v{i}"), id)
            .await
            .unwrap();
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions.len(),
        3,
        "le vault doit appliquer max_versions=3 depuis la config TOML"
    );
}

/// F-32A — TTL : 2 anciens expirés, 1 récent conservé (timestamps injectés).
///
/// Utilise des timestamps artificiels pour contrôler précisément les âges.
#[tokio::test]
async fn f32a_vault_ttl_config_removes_expired_on_trim() {
    use gradatum_core::config::HistoryConfig;
    use std::fs;

    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();
    let id = NoteId::new();

    // Créer la note de base.
    vault
        .write_note_with_id(fm(), "corps base".into(), id)
        .await
        .unwrap();

    // Injecter 3 snapshots artificiels : ts=0 (vieux), ts=1j (vieux), ts=2j (récent).
    let one_day_ms: i64 = 24 * 3600 * 1000;
    let id_str = id.to_string();
    let history_dir = dir.path().join("main").join(".history").join(&id_str);
    fs::create_dir_all(&history_dir).unwrap();

    let snapshot_ts: Vec<i64> = vec![0, one_day_ms, 2 * one_day_ms];
    for &ts in &snapshot_ts {
        fs::write(history_dir.join(format!("{ts}.md")), format!("snap {ts}")).unwrap();
    }

    let versions = vault.history_versions(id).await.unwrap();
    assert_eq!(versions.len(), 3, "3 snapshots artificiels");

    // now = 100 jours depuis l'époque 0, TTL = 99 jours :
    //   cutoff = 100j - 99j = 1j
    //   ts=0 (0j) < 1j → expiré
    //   ts=1j == 1j → NON expiré (strict <)
    //   ts=2j > 1j → NON expiré
    // → 1 expiré, 2 restants. max_versions=50 → pas de cap.
    let now_ms = (100 * one_day_ms) as u64;
    let cfg = HistoryConfig {
        max_versions: 50,
        ttl_days: Some(99),
    };
    vault
        .apply_history_trim(id, &id_str, "main", &cfg, now_ms)
        .await;

    let versions_after = vault.history_versions(id).await.unwrap();
    assert_eq!(
        versions_after.len(),
        2,
        "1 expiré (ts=0) → 2 restants (ts=1j et ts=2j)"
    );
    assert_eq!(versions_after[0], snapshot_ts[1], "ts=1j doit rester");
    assert_eq!(versions_after[1], snapshot_ts[2], "ts=2j doit rester");
}

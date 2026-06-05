//! Contrat de stockage et recherche full-text + overrides + checksums.
//!
//! [`IndexStore`] expose les opérations d'indexation (FTS5), de gestion des overrides
//! génériques et du drift detection. Il est un sous-trait du [`Index`](crate::index::Index)
//! historique, conçu pour être consommé par les futurs pipelines de recherche sans
//! dépendre de `gradatum-index`.
//!
//! ## Évolution v0.4.0
//!
//! L'erreur `GradatumError` convergera vers un `StoreError` dédié (cf. `QueueStore`) à v0.4.0.

use async_trait::async_trait;

use crate::error::GradatumError;
use crate::identity::NoteId;
use crate::index::{FileChecksumEntry, NoteRecord};
use crate::scope::{OverrideScope, VaultId};

// ── Types publics migrés depuis gradatum-index (Étape 0.2a) ───────────────────

/// Résultat brut d'une recherche FTS5 avec snippet.
///
/// Retourné par [`IndexStore::search_fts_with_snippet`] — contient le snippet
/// FTS5 natif localisé sur le terme recherché (vs `build_snippet` qui tronque
/// la tête du body).
///
/// Migré depuis `gradatum-index::sqlite::SearchHitRaw` à l'Étape 0.2a pour
/// permettre l'exposition via le trait `IndexStore` (object-safe).
#[derive(Debug, Clone)]
pub struct SearchHitRaw {
    /// Identifiant ULID de la note.
    pub note_id: NoteId,
    /// Score BM25 brut (valeur négative — meilleur match plus proche de 0).
    pub bm25: f64,
    /// Statut de la note (`"live"`, `"downgraded"`, etc.).
    pub status: String,
    /// Snippet FTS5 natif localisé (`snippet(notes_fts, 0, '»', '«', '...', 32)`).
    pub snippet: String,
    /// Section de la note (ex. `"decisions"`, `"reference"`).
    pub section: String,
    /// Titre H1 de la note (extrait post-curate via migration 0005, peut être absent).
    pub title: Option<String>,
}

/// Entrée auteur retournée par [`IndexStore::distinct_authors`].
///
/// Migré depuis `gradatum-index::queries::AuthorRow` à l'Étape 0.2a pour
/// permettre l'exposition via le trait `IndexStore` (object-safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorRow {
    /// Nom affiché de l'auteur (`author_display_name` si défini, sinon `author_id`).
    pub name: String,
    /// Nombre de notes attribuées à cet auteur.
    pub note_count: u64,
}

/// Lignée d'une note : parents (backlinks) et enfants (forward links).
///
/// Retournée par [`IndexStore::trace_lineage`].
///
/// Migré depuis `gradatum-index::queries::Lineage` à l'Étape 0.2a.
#[derive(Debug, Clone, Default)]
pub struct Lineage {
    /// Identifiants ULID des notes qui lient vers cette note (backlinks).
    pub parents: Vec<String>,
    /// Identifiants ULID des notes vers lesquelles cette note lie.
    pub children: Vec<String>,
}

/// Contrat de stockage full-text, overrides et checksums — async, thread-safe.
///
/// Implémenté par `gradatum-index::SqliteIndex` (Phase 1).
///
/// ## Stabilité
///
/// `#[stability::unstable]` — l'API peut changer jusqu'à Silver (v1.0.0).
/// L'erreur `GradatumError` convergera vers `StoreError` dédié (cf. `QueueStore`) à v0.4.0.
///
/// ## Contention
///
/// En v0.3.0, ce trait partage un `Arc<Mutex<Connection>>` unique avec `DocumentStore`
/// et `VectorStore`. Séparation physique des connexions prévue à v0.4.0.
// AM1 : instabilité documentée ici et dans le module doc.
// `#[stability::unstable]` différé v0.4.0 — nécessite `[features] unstable-storage-traits = []`
// dans gradatum-core/Cargo.toml + opt-in de tous les consommateurs workspace.
// La macro stability n'empêche rien (pas d'E0365) ; sans la feature déclarée elle émettrait
// un deprecated warning sur chaque consommateur.
#[async_trait]
pub trait IndexStore: Send + Sync {
    /// Recherche plein texte dans un vault.
    ///
    /// Retourne les `NoteId` correspondants triés par pertinence descendante.
    /// `limit` est le nombre maximum de résultats.
    async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError>;

    /// Recherche FTS5 retournant les ids triés par BM25 + score réel + status.
    ///
    /// Le score est la valeur `bm25(notes_fts)` (négative — meilleur match
    /// = plus proche de 0). Ordre cohérent avec `search_fts` (ASC par score).
    ///
    /// Retourne des triplets `(NoteId, score_bm25, status)` triés du meilleur
    /// au moins bon match.
    ///
    /// Phase 2.1.2 alpha.9 — param `include_downgraded` :
    /// - `false` (défaut) : exclut les notes avec `status = 'downgraded'`.
    /// - `true` : inclut les notes downgraded avec un score BM25 multiplié par 0.1
    ///   (pénalité de pertinence — elles apparaissent en dernier).
    async fn search_fts_scored(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError>;

    /// Insère ou met à jour un override dans la table générique `note_overrides`.
    ///
    /// La clé est `(note_id, scope, override_type)` — 1 override actif par tuple (décision Q7).
    /// `payload_toml` est le payload sérialisé via `OverridePayload::to_toml()`.
    async fn upsert_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
        schema_version: u32,
        payload_toml: &str,
    ) -> Result<(), GradatumError>;

    /// Récupère un override depuis la table générique.
    ///
    /// Retourne `(schema_version, payload_toml)` ou `None` si absent.
    async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError>;

    /// Insère ou met à jour une entrée de checksum de fichier.
    ///
    /// Utilisé par le drift detector pour tracker l'état attendu des fichiers Markdown.
    async fn upsert_file_checksum(&self, entry: &FileChecksumEntry) -> Result<(), GradatumError>;

    /// Liste toutes les entrées de checksum de fichiers.
    ///
    /// Utilisé par le drift detector lors d'un scan complet du vault.
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError>;

    /// Retourne `(created_ms, in_degree)` pour une note.
    ///
    /// `created_ms` : timestamp de création en millisecondes epoch Unix.
    /// `in_degree` : nombre de backlinks entrants (liens wikilinks pointant vers cette note).
    ///
    /// Un backend sans table de liens PEUT retourner `(created_ms, 0)` pour `in_degree`.
    ///
    /// # Erreurs
    ///
    /// - `GradatumError::NoteNotFound` si la note est absente.
    /// - `GradatumError::Storage` si la requête échoue ou si `note_id` n'est pas un ULID valide.
    async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError>;

    // ── Méthodes promues à l'Étape 0.2a (dyn-wiring) ─────────────────────────

    /// Recherche FTS5 avec snippet FTS5 natif et filtre section optionnel.
    ///
    /// Retourne [`SearchHitRaw`] qui inclut le snippet, la section, le titre et le score BM25.
    ///
    /// # Paramètres
    ///
    /// - `vault_id` : identifiant du vault (ex. `VaultId::new("main")`).
    /// - `query` : requête FTS5 normalisée (via `build_fts_query`).
    /// - `limit` : nombre max de résultats.
    /// - `include_downgraded` : si `false`, exclut les notes `status='downgraded'`.
    /// - `section` : filtre section optionnel (`None` = toutes sections).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError>;

    /// Cherche une note par son titre Markdown (première ligne `# {title}`).
    ///
    /// Retourne l'identifiant ULID de la première note trouvée, ou `None`.
    /// Exclut les notes `status != 'live'` (sémantique legacy vault : notes archivées
    /// non adressables par titre).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError>;

    /// Compte les notes `status = 'live'` pour un vault.
    ///
    /// Exclut les sentinelles (`id NOT LIKE '__sentinel__%'`).
    /// Utilisé par `vault_status.note_count`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError>;

    /// Liste les auteurs distincts d'un vault avec leur nombre de notes.
    ///
    /// Exclut les sentinelles et les notes sans auteur.
    /// Retourne `name` = `author_display_name` si défini, sinon `author_id`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError>;

    /// Liste les tags distincts d'un vault avec leur fréquence.
    ///
    /// Retourne `Vec<(tag, count)>` trié par fréquence décroissante.
    /// Les tags sont agrégés côté Rust (split espace depuis `notes.tags`).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError>;

    /// Retourne les voisins d'une note jusqu'à `depth` niveaux (max 3).
    ///
    /// Utilise un CTE récursif BFS sur `note_links`. La note source est exclue du résultat.
    /// `depth` est plafonné à 3 pour éviter une traversée runaway.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête CTE échoue.
    async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError>;

    /// Retourne les backlinks (notes qui lient vers `note_id`) pour un vault.
    ///
    /// Nécessite la table `note_links` (migration 0002).
    /// Retourne une liste d'identifiants ULID (`src_note_id`) qui pointent vers `note_id`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn backlinks(&self, vault_id: &str, note_id: &str) -> Result<Vec<String>, GradatumError>;

    /// Retourne la lignée d'une note : parents (backlinks) et enfants (forward links).
    ///
    /// Combine deux requêtes sur `note_links` :
    /// - `parents` = notes qui pointent vers `note_id`.
    /// - `children` = notes vers lesquelles `note_id` pointe.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si l'une des requêtes échoue.
    async fn trace_lineage(&self, vault_id: &str, note_id: &str) -> Result<Lineage, GradatumError>;

    /// Liste les notes d'un vault avec pagination par curseur ULID.
    ///
    /// Retourne `(records, total)` — `total` est le comptage absolu (pour `X-Total-Count`).
    /// `cursor` = dernier ULID reçu (exclusif) ; `None` = début de liste.
    /// `section` = filtre optionnel sur la section.
    /// Notes downgraded exclues.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError>;

    /// Somme totale de `LENGTH(body_text)` pour les notes non-sentinelles d'un vault.
    ///
    /// Retourne 0 si aucune note. Utilisé par `vault_status.total_size_bytes`.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError>;

    /// Insère ou ignore un lien wikilink entre deux notes.
    ///
    /// Idempotent (`INSERT OR IGNORE`). Utilisé par le curator pour enregistrer
    /// les liens `[[...]]` détectés dans le body de la note.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête échoue.
    async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError>;

    /// Récupère `title` et `section` en batch pour une liste d'identifiants ULID.
    ///
    /// Utilisé par le handler `vault_search` pour enrichir les hits sémantique-only
    /// (présents dans le résultat RRF fusionné mais absents de la map BM25) avec
    /// leurs métadonnées `title` et `section`.
    ///
    /// ## Comportement
    ///
    /// - 1 seul SELECT `id, title, section FROM notes WHERE vault_id = ? AND id IN (…)`.
    /// - Les identifiants absents de la table `notes` ne figurent pas dans le résultat.
    /// - Sentinelles (id LIKE `__sentinel__%`) exclues.
    ///
    /// ## Retour
    ///
    /// `HashMap<note_id, (title, section)>` — `title` est `None` si la colonne est
    /// NULL (note antérieure à la migration 0009 sans H1).
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si la requête SQLite échoue.
    async fn get_titles_sections(
        &self,
        vault_id: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError>;
}

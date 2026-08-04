//! Wikilink redirect table (`redirect_table`).
//!
//! When a note is renamed, its old title slug is inserted into
//! `redirect_table` (old_slug → ulid). Wikilink resolution can then
//! locate the note despite its title change.
//!
//! ## Slug normalisation
//!
//! `title_to_slug(s)` = lowercase + trim + spaces/multiple-dashes → single dash.
//! No external dependency: ASCII normalisation is sufficient for gradatum titles
//! (Latin + digits, consistent with vault naming conventions).
//!
//! ## Pattern
//!
//! The `redirect_table` is created by migration `0010_provenance_trust_redirects`.
//! Methods are exposed as inherent methods on `SqliteIndex` (via delegation)
//! and wired in `index_store_impl.rs` for the `IndexStore` trait.

use ulid::Ulid;

use gradatum_core::error::GradatumError;

use crate::SqliteIndex;

// ── Slug normalisation ─────────────────────────────────────────────────────────

/// Normalises a title into a slug (lowercase, spaces → dashes, multiple dashes collapsed).
///
/// Used to generate primary keys for `redirect_table`.
///
/// # Examples
///
/// ```
/// use gradatum_index::links::title_to_slug;
/// assert_eq!(title_to_slug("Mon Titre"), "mon-titre");
/// assert_eq!(title_to_slug("  Espaces   Partout  "), "espaces-partout");
/// assert_eq!(title_to_slug("A--B  C"), "a-b-c");
/// ```
pub fn title_to_slug(title: &str) -> String {
    // 1. Lowercase
    let lower = title.to_lowercase();
    // 2. Trim bords
    let trimmed = lower.trim();
    // 3. Espaces et tirets multiples → tiret unique
    let mut slug = String::with_capacity(trimmed.len());
    let mut last_was_dash = false;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '-' {
            if !last_was_dash && !slug.is_empty() {
                slug.push('-');
                last_was_dash = true;
            }
        } else {
            slug.push(ch);
            last_was_dash = false;
        }
    }
    // Supprimer le tiret final éventuel (cas "titre-")
    if slug.ends_with('-') {
        slug.pop();
    }
    slug
}

// ── SqliteIndex inherent methods ──────────────────────────────────────────────

impl SqliteIndex {
    /// Inserts or updates a redirect `old_slug → ulid`, scoped to `vault_id`.
    ///
    /// `vault_id`: namespace of the redirect — part of the composite primary key
    /// `(vault_id, title_slug)` (migration 0035). Two vaults may register the same
    /// slug without clobbering each other.
    /// `renamed_at_ms`: rename timestamp in Unix epoch milliseconds.
    ///
    /// Idempotent: `INSERT OR REPLACE` — a double rename of the same `(vault_id, slug)`
    /// is tolerated (the last ULID wins). This is intentional: if a note is renamed
    /// twice, only the latest current ULID is relevant.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the INSERT fails.
    pub async fn upsert_redirect(
        &self,
        vault_id: &str,
        slug: &str,
        ulid: &Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        let ulid_str = ulid.to_string();
        conn.execute(
            "INSERT OR REPLACE INTO redirect_table (vault_id, title_slug, ulid, renamed_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![vault_id, slug, ulid_str, renamed_at_ms],
        )
        .map_err(|e| GradatumError::Storage(format!("upsert_redirect : {e}")))?;
        Ok(())
    }

    /// Looks up the current ULID for an old title slug within `vault_id`.
    ///
    /// Scoped to `vault_id` (composite PK `(vault_id, title_slug)`, migration 0035):
    /// a slug registered in another vault is never resolved here.
    ///
    /// Returns `Some(ulid)` if the slug exists in `redirect_table` for this vault,
    /// `None` otherwise.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    pub async fn lookup_redirect(
        &self,
        vault_id: &str,
        slug: &str,
    ) -> Result<Option<Ulid>, GradatumError> {
        let conn = self.conn.lock().await;
        match conn.query_row(
            "SELECT ulid FROM redirect_table WHERE vault_id = ?1 AND title_slug = ?2",
            rusqlite::params![vault_id, slug],
            |row| row.get::<_, String>(0),
        ) {
            Ok(ulid_str) => {
                let ulid = Ulid::from_string(&ulid_str).map_err(|e| {
                    GradatumError::Storage(format!("redirect_table ulid parse : {e}"))
                })?;
                Ok(Some(ulid))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(GradatumError::Storage(format!("lookup_redirect : {e}"))),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::IndexStore;
    use std::sync::Arc;

    #[test]
    fn title_to_slug_basic() {
        assert_eq!(title_to_slug("Mon Titre"), "mon-titre");
        assert_eq!(title_to_slug("  Espaces   Partout  "), "espaces-partout");
        assert_eq!(title_to_slug("A--B  C"), "a-b-c");
        assert_eq!(title_to_slug("Hello"), "hello");
        assert_eq!(title_to_slug(""), "");
    }

    #[test]
    fn title_to_slug_trailing_dash() {
        // Pas de tiret final
        assert_eq!(title_to_slug("titre-"), "titre");
    }

    #[tokio::test]
    async fn redirect_resolves_old_title_to_same_ulid() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let ulid = Ulid::new();

        // Upsert d'un redirect
        idx.upsert_redirect("main", "ancien-titre", &ulid, 1_700_000_000_000)
            .await
            .expect("upsert_redirect ne doit pas échouer");

        // Résolution via trait IndexStore
        let store: Arc<dyn IndexStore> = Arc::new(idx);
        let found = store
            .resolve_redirect("main", "ancien-titre")
            .await
            .expect("resolve_redirect ne doit pas échouer");
        assert_eq!(found, Some(ulid), "le ULID résolu doit correspondre");

        // Slug inconnu → None
        let none = store
            .resolve_redirect("main", "inconnu")
            .await
            .expect("resolve_redirect slug inconnu ne doit pas échouer");
        assert!(none.is_none(), "slug inconnu → None");
    }

    #[tokio::test]
    async fn upsert_redirect_idempotent_last_wins() {
        let idx = SqliteIndex::open_in_memory()
            .await
            .expect("open_in_memory ne doit pas échouer");
        let ulid1 = Ulid::new();
        let ulid2 = Ulid::new();

        idx.upsert_redirect("main", "titre-a", &ulid1, 1_000)
            .await
            .expect("upsert 1");
        idx.upsert_redirect("main", "titre-a", &ulid2, 2_000)
            .await
            .expect("upsert 2 — replace");

        let found = idx
            .lookup_redirect("main", "titre-a")
            .await
            .expect("lookup_redirect ne doit pas échouer");
        assert_eq!(found, Some(ulid2), "le second upsert remplace le premier");
    }
}

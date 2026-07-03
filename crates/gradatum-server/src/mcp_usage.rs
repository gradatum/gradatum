//! Compteurs d'usage des outils MCP — télémétrie feat/usage-telemetry-19091.
//!
//! ## Design
//!
//! `McpToolCounters` est une map fermée pré-peuplée à l'init (21 clés, une par outil MCP).
//! Les `AtomicU64` gèrent l'incrément concurrent sans verrou sur la map.
//! `record(name)` fait un simple `map.get(name)` — un nom inconnu est un **no-op**
//! (garantit la cardinalité bornée à 21 outils MCP, quelle que soit l'entrée).
//!
//! ## Usage
//!
//! - `record(tool)` appelé en tête de `dispatch_tool()` (call site unique, P1-1).
//! - `swap_all(window_h)` appelé dans la tâche flush 60s pour produire les [`UsageFlushEntry`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::read_usage_store::UsageFlushEntry;

/// 21 paires `(nom_outil, clé_endpoint)` définissant la map fermée.
///
/// Le préfixe `mcp:` namespaced les clés pour les distinguer des 5 read-paths
/// HTTP dans la table `read_usage_counters` et dans les familles Prometheus.
///
/// Ordre identique aux arms de `dispatch_tool()` dans `mcp.rs`.
pub const MCP_TOOL_KEYS: &[(&str, &str)] = &[
    ("vault_status", "mcp:vault_status"),
    ("vault_authors", "mcp:vault_authors"),
    ("vault_tags", "mcp:vault_tags"),
    ("vault_search", "mcp:vault_search"),
    ("vault_read", "mcp:vault_read"),
    ("vault_list", "mcp:vault_list"),
    ("vault_graph", "mcp:vault_graph"),
    ("vault_links", "mcp:vault_links"),
    ("vault_trace", "mcp:vault_trace"),
    ("vault_context", "mcp:vault_context"),
    ("vault_timeline", "mcp:vault_timeline"),
    ("vault_lessons_recall", "mcp:vault_lessons_recall"),
    ("vault_write", "mcp:vault_write"),
    ("vault_classify", "mcp:vault_classify"),
    ("vault_downgrade", "mcp:vault_downgrade"),
    ("vault_history", "mcp:vault_history"),
    ("vault_history_get", "mcp:vault_history_get"),
    ("vault_restore", "mcp:vault_restore"),
    ("vault_diff", "mcp:vault_diff"),
    ("vault_forget", "mcp:vault_forget"),
    ("code_scope", "mcp:code_scope"),
];

/// Compteurs atomiques par outil MCP — map fermée, pré-peuplée, lecture seule après `new()`.
///
/// `Arc`-able et cloneable — injecté dans `AppState` et partagé entre les handlers
/// MCP et la tâche flush 60s.
#[derive(Clone)]
pub struct McpToolCounters {
    /// Map fermée : nom outil → compteur AtomicU64.
    ///
    /// Wrappé dans `Arc` pour que le clone de `McpToolCounters` partage les mêmes
    /// compteurs (pas des copies).
    map: Arc<HashMap<&'static str, AtomicU64>>,
}

impl McpToolCounters {
    /// Crée un jeu de compteurs pré-peuplé avec les 21 outils de `MCP_TOOL_KEYS`.
    ///
    /// La map est en lecture seule après `new()` — aucun verrou nécessaire sur le `get`.
    /// Les `AtomicU64` sont initialisés à 0.
    #[must_use]
    pub fn new() -> Self {
        let map: HashMap<&'static str, AtomicU64> = MCP_TOOL_KEYS
            .iter()
            .map(|(tool, _key)| (*tool, AtomicU64::new(0)))
            .collect();
        Self { map: Arc::new(map) }
    }

    /// Incrémente le compteur de l'outil `tool`.
    ///
    /// Si `tool` est inconnu (absent de la map fermée) → **no-op** (cardinalité bornée).
    /// Utilise `Ordering::Relaxed` — seule la valeur du compteur compte, pas l'ordering
    /// cross-thread (même logique que `ReadUsageAccumulators`).
    pub fn record(&self, tool: &str) {
        if let Some(counter) = self.map.get(tool) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Swap-reset tous les compteurs et retourne les deltas sous forme de [`UsageFlushEntry`].
    ///
    /// Chaque entrée a `endpoint = mcp:<tool>` et `hit_count` = delta depuis le dernier swap.
    /// Les entrées avec `hit_count = 0` sont incluses — `flush_batch` les filtre.
    ///
    /// `window_h = epoch_ms / 3_600_000` (fourni par l'appelant — tâche flush 60s).
    ///
    /// # Important
    ///
    /// Le swap est atomique par compteur (pas atomique entre tous les compteurs).
    /// Acceptable : la fenêtre horaire est de 60s, et un hit perdu entre deux swap
    /// sur des compteurs différents est acceptable pour la télémétrie.
    ///
    /// Câblé dans la tâche flush 60s (`main.rs`).
    #[allow(dead_code)]
    pub fn swap_all(&self, window_h: i64) -> Vec<UsageFlushEntry> {
        MCP_TOOL_KEYS
            .iter()
            .map(|(tool, mcp_key)| {
                // SAFETY: la map a été peuplée avec exactement MCP_TOOL_KEYS au `new()`.
                // Un `get` ici ne peut pas échouer — le tool existe toujours dans la map.
                let hit_count = self
                    .map
                    .get(*tool)
                    .expect("MCP_TOOL_KEYS invariant : clé toujours présente dans la map")
                    .swap(0, Ordering::Relaxed);
                UsageFlushEntry {
                    // SAFETY: `mcp_key` est `&'static str` — issu de `MCP_TOOL_KEYS` littéral.
                    endpoint: mcp_key,
                    window_h,
                    hit_count,
                }
            })
            .collect()
    }

    /// Helper test : retourne les paires `(mcp_key, hit_count)` sans reset.
    ///
    /// Utilisé dans les tests unitaires pour inspecter l'état des compteurs.
    /// NE PAS utiliser en production (pas de reset → double-compte si suivi de `swap_all`).
    #[cfg(test)]
    pub fn swap_all_for_test(&self) -> Vec<(&'static str, u64)> {
        MCP_TOOL_KEYS
            .iter()
            .map(|(tool, mcp_key)| {
                let count = self
                    .map
                    .get(*tool)
                    .expect("MCP_TOOL_KEYS invariant")
                    .swap(0, Ordering::Relaxed);
                (*mcp_key, count)
            })
            .collect()
    }
}

impl Default for McpToolCounters {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests unitaires
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper : retrouver hit_count pour une mcp_key dans le résultat de swap_all_for_test.
    fn get_hit(entries: &[(&str, u64)], key: &str) -> u64 {
        entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// La map contient exactement 21 outils ; `record` incrémente le bon compteur.
    #[test]
    fn mcp_counters_has_21_tools_and_records() {
        let c = McpToolCounters::new();
        c.record("vault_list");
        c.record("vault_list");
        c.record("code_scope");
        // swap renvoie les deltas ; vault_list=2, code_scope=1, autres=0
        let entries = c.swap_all_for_test();
        assert_eq!(MCP_TOOL_KEYS.len(), 21);
        assert_eq!(get_hit(&entries, "mcp:vault_list"), 2);
        assert_eq!(get_hit(&entries, "mcp:code_scope"), 1);
        assert_eq!(get_hit(&entries, "mcp:vault_write"), 0);
    }

    /// Un nom inconnu est un no-op — aucun compteur ne doit être affecté.
    #[test]
    fn mcp_counters_unknown_tool_is_noop() {
        let c = McpToolCounters::new();
        c.record("outil_inexistant");
        let entries = c.swap_all_for_test();
        assert!(entries.iter().all(|(_, n)| *n == 0));
    }

    /// `swap_all` produit des `UsageFlushEntry` avec la bonne `window_h`.
    #[test]
    fn swap_all_produces_entries_with_window_h() {
        let c = McpToolCounters::new();
        c.record("vault_search");
        let entries = c.swap_all(42);
        // Toutes les entries doivent avoir window_h = 42.
        assert!(entries.iter().all(|e| e.window_h == 42));
        // vault_search doit avoir hit_count = 1.
        let vs = entries
            .iter()
            .find(|e| e.endpoint == "mcp:vault_search")
            .expect("mcp:vault_search doit exister");
        assert_eq!(vs.hit_count, 1);
    }

    /// Après `swap_all`, les compteurs sont remis à zéro.
    #[test]
    fn swap_all_resets_counters() {
        let c = McpToolCounters::new();
        c.record("vault_write");
        let _ = c.swap_all(1);
        // Deuxième swap → tous à 0.
        let entries = c.swap_all(2);
        assert!(entries.iter().all(|e| e.hit_count == 0));
    }
}

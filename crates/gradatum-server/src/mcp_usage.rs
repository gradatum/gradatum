//! Compteurs d'usage des outils MCP — télémétrie feat/usage-telemetry-19091.
//!
//! ## Design
//!
//! `McpToolCounters` est une map fermée pré-peuplée à l'init (une clé par outil MCP
//! instrumenté — voir `MCP_TOOL_KEYS`).
//! Elle DOIT couvrir tous les outils déclarés par `tool_catalog()` : cette parité est
//! garantie par le test `every_declared_tool_is_instrumented` (`api_v1::mcp::tests`),
//! qui compare les deux sources et rougit sur tout outil exposé mais non compté (F-234).
//! Les `AtomicU64` gèrent l'incrément concurrent sans verrou sur la map.
//! `record(name)` fait un simple `map.get(name)` — un nom inconnu est un **no-op**
//! (garantit la cardinalité bornée au nombre d'entrées de `MCP_TOOL_KEYS`, quelle que
//! soit l'entrée).
//!
//! ## Usage
//!
//! - `record(tool)` appelé en tête de `dispatch_tool()` (call site unique, P1-1).
//! - `swap_all(window_h)` appelé dans la tâche flush 60s pour produire les [`UsageFlushEntry`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::read_usage_store::UsageFlushEntry;

/// Paires `(nom_outil, clé_endpoint)` définissant la map fermée — une par outil MCP
/// instrumenté.
///
/// Le préfixe `mcp:` namespaced les clés pour les distinguer des 5 read-paths
/// HTTP dans la table `read_usage_counters` et dans les familles Prometheus.
///
/// Ordre calé sur les arms de `dispatch_tool()` dans `mcp.rs`. La complétude vis-à-vis
/// de `tool_catalog()` n'est PAS supposée depuis cet ordre : elle est prouvée par le
/// test de parité `every_declared_tool_is_instrumented` (F-234).
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
    ("vault_archives_list", "mcp:vault_archives_list"),
    ("code_scope", "mcp:code_scope"),
    ("create_feature_card", "mcp:create_feature_card"),
    // F-234 : capacités exposées par `dispatch_tool` mais historiquement hors compteur
    // (`vault_proactive_recall`/`_feedback` appelées à chaque recall/écriture, `job_status`
    // à chaque confirmation de job). Désormais instrumentées — aucune exclusion. La parité
    // avec `tool_catalog()` est verrouillée par `every_declared_tool_is_instrumented`.
    ("vault_proactive_recall", "mcp:vault_proactive_recall"),
    (
        "vault_proactive_recall_feedback",
        "mcp:vault_proactive_recall_feedback",
    ),
    ("job_status", "mcp:job_status"),
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
    /// Crée un jeu de compteurs pré-peuplé avec tous les outils de `MCP_TOOL_KEYS`.
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
                    .expect("MCP_TOOL_KEYS invariant: key always present in the map")
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

    /// `record` incrémente le bon compteur ; les autres restent à zéro.
    ///
    /// Le CARDINAL de la map (autrefois `assert_eq!(MCP_TOOL_KEYS.len(), 23)`) n'est
    /// volontairement plus gravé ici : un compte en dur dérive au premier ajout et
    /// reste vert sur un renommage. La complétude de l'instrumentation est prouvée par
    /// parité de sources dans `every_declared_tool_is_instrumented`
    /// (`api_v1::mcp::tests`) — pas par un nombre (F-234).
    #[test]
    fn mcp_counters_record_and_swap_deltas() {
        let c = McpToolCounters::new();
        c.record("vault_list");
        c.record("vault_list");
        c.record("code_scope");
        // swap renvoie les deltas ; vault_list=2, code_scope=1, autres=0
        let entries = c.swap_all_for_test();
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

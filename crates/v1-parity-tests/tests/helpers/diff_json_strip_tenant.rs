//! Helpers de comparaison JSON — parité v1 vs gradatum alpha.
//!
//! # Rôle
//!
//! - [`strip_tenant`] — supprime récursivement les champs propres à gradatum
//!   absents du legacy vault v1.6.2 (tenant_id, _gradatum_*).
//! - [`diff_json`] — diff structurel récursif ; retourne les chemins divergents.
//!
//! # Usage Phase 2.1
//!
//! Ces helpers seront utilisés en Phase 2.1 avec `migrate-from-v0` pour la
//! parité contenu stricte (diff JSON nul sur les 10 méthodes read).
//!

use serde_json::Value;

/// Supprime récursivement les champs gradatum-only dans un `Value` JSON.
///
/// Champs supprimés :
/// - `tenant_id` (gradatum multi-tenant — absent du legacy vault v1.6.2)
/// - `_gradatum_*` (méta-champs internes — préfixe réservé)
///
/// Opère en place. Si `v` est un objet, les champs sont supprimés à ce niveau
/// puis la récursion descend dans les valeurs restantes. Si `v` est un tableau,
/// la récursion descend dans chaque élément.
pub fn strip_tenant(v: &mut Value) {
    match v {
        Value::Object(map) => {
            // Collecter les clés à supprimer avant mutation.
            let to_remove: Vec<String> = map
                .keys()
                .filter(|k| *k == "tenant_id" || k.starts_with("_gradatum_"))
                .cloned()
                .collect();
            for key in to_remove {
                map.remove(&key);
            }
            // Récursion dans les valeurs restantes.
            for child in map.values_mut() {
                strip_tenant(child);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                strip_tenant(item);
            }
        }
        // Scalaires : rien à faire.
        _ => {}
    }
}

/// Diff structurel récursif entre deux valeurs JSON.
///
/// Retourne une liste de chemins (notation pointée) où les valeurs diffèrent.
/// Une liste vide signifie que les deux valeurs sont structurellement identiques.
///
/// # Sémantique
///
/// - Clé présente dans `left` mais absente dans `right` → `"<path>: missing in right"`
/// - Clé présente dans `right` mais absente dans `left` → `"<path>: missing in left"`
/// - Scalaires différents → `"<path>: <left_debug> != <right_debug>"`
/// - Tableaux : comparaison élément par élément (indices comme sous-chemins)
///
/// # Note
///
/// Les tableaux de longueur différente génèrent des différences d'éléments
/// manquants. Les types différents (objet vs scalaire) génèrent une différence
/// directe si `l != r`.
pub fn diff_json(left: &Value, right: &Value) -> Vec<String> {
    let mut diffs = Vec::new();
    diff_inner("", left, right, &mut diffs);
    diffs
}

/// Implémentation récursive du diff.
fn diff_inner(path: &str, l: &Value, r: &Value, diffs: &mut Vec<String>) {
    match (l, r) {
        (Value::Object(la), Value::Object(ra)) => {
            // Clés présentes dans left.
            for (k, lv) in la {
                let sub = child_path(path, k);
                match ra.get(k) {
                    Some(rv) => diff_inner(&sub, lv, rv, diffs),
                    None => diffs.push(format!("{sub}: missing in right")),
                }
            }
            // Clés présentes uniquement dans right.
            for k in ra.keys() {
                if !la.contains_key(k) {
                    let sub = child_path(path, k);
                    diffs.push(format!("{sub}: missing in left"));
                }
            }
        }
        (Value::Array(la), Value::Array(ra)) => {
            let max_len = la.len().max(ra.len());
            for i in 0..max_len {
                let sub = format!("{path}[{i}]");
                match (la.get(i), ra.get(i)) {
                    (Some(lv), Some(rv)) => diff_inner(&sub, lv, rv, diffs),
                    (Some(_), None) => diffs.push(format!("{sub}: missing in right")),
                    (None, Some(_)) => diffs.push(format!("{sub}: missing in left")),
                    (None, None) => unreachable!("i < max_len"),
                }
            }
        }
        // Scalaires identiques → rien.
        (l, r) if l == r => {}
        // Scalaires différents ou types incompatibles.
        (l, r) => diffs.push(format!("{path}: {l:?} != {r:?}")),
    }
}

/// Construit le chemin enfant pour le diff (notation pointée).
fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_tenant_removes_tenant_id() {
        let mut v = json!({
            "path": "decisions/test",
            "tenant_id": "main",
            "content": "body"
        });
        strip_tenant(&mut v);
        assert!(v.get("tenant_id").is_none(), "tenant_id doit être supprimé");
        assert!(v.get("path").is_some(), "path doit être conservé");
    }

    #[test]
    fn strip_tenant_removes_gradatum_prefix() {
        let mut v = json!({
            "path": "decisions/test",
            "_gradatum_version": 1,
            "_gradatum_schema": "v1"
        });
        strip_tenant(&mut v);
        assert!(v.get("_gradatum_version").is_none());
        assert!(v.get("_gradatum_schema").is_none());
        assert!(v.get("path").is_some());
    }

    #[test]
    fn strip_tenant_recursive_in_array() {
        let mut v = json!({
            "items": [
                {"path": "a", "tenant_id": "main"},
                {"path": "b", "tenant_id": "main"}
            ]
        });
        strip_tenant(&mut v);
        let items = v["items"].as_array().unwrap();
        for item in items {
            assert!(item.get("tenant_id").is_none());
            assert!(item.get("path").is_some());
        }
    }

    #[test]
    fn diff_json_empty_on_equal() {
        let a = json!({"key": "value", "num": 42});
        let b = json!({"key": "value", "num": 42});
        let diffs = diff_json(&a, &b);
        assert!(diffs.is_empty(), "pas de diff sur valeurs égales: {diffs:?}");
    }

    #[test]
    fn diff_json_detects_value_mismatch() {
        let a = json!({"key": "value1"});
        let b = json!({"key": "value2"});
        let diffs = diff_json(&a, &b);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("key"), "path doit contenir 'key'");
    }

    #[test]
    fn diff_json_detects_missing_in_right() {
        let a = json!({"key": "value", "extra": "present"});
        let b = json!({"key": "value"});
        let diffs = diff_json(&a, &b);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("missing in right"));
    }

    #[test]
    fn diff_json_detects_missing_in_left() {
        let a = json!({"key": "value"});
        let b = json!({"key": "value", "new_field": "x"});
        let diffs = diff_json(&a, &b);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("missing in left"));
    }

    #[test]
    fn diff_json_nested_path() {
        let a = json!({"outer": {"inner": "a"}});
        let b = json!({"outer": {"inner": "b"}});
        let diffs = diff_json(&a, &b);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("outer.inner"), "path pointé attendu: {}", diffs[0]);
    }

    #[test]
    fn strip_then_diff_equal_ignoring_tenant() {
        let mut a = json!({"path": "x", "content": "body", "tenant_id": "main"});
        let mut b = json!({"path": "x", "content": "body"});
        strip_tenant(&mut a);
        strip_tenant(&mut b);
        let diffs = diff_json(&a, &b);
        assert!(diffs.is_empty(), "après strip, diff doit être vide: {diffs:?}");
    }
}

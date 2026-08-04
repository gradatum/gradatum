//! HARDEN-CONFIG défaut 2 — `retention_days` doit refuser la valeur `0`.
//!
//! Invariant visé : `retention_days = 0` fixe le cutoff de `cleanup_dlq_daily` à
//! « maintenant », et `DELETE FROM gradatum_jobs WHERE status='DLQ'` purge alors la
//! **totalité** du DLQ, irréversiblement et sans confirmation. Les 30 jours sont un
//! *défaut serde* (appliqué seulement quand la clé est absente), pas un plancher.
//!
//! Le refus est posé au **parse** et non dans une méthode `validate()` : une méthode ne
//! vaut que par ses sites d'appel, et c'est précisément cette asymétrie garde/usage que
//! ce lot corrige. Au parse, aucune `ScheduleConfig` avec `retention_days = 0` ne peut
//! exister.

use gradatum_worker::ApalisConfig;

/// `[[schedules]]` minimal, avec ou sans clé `retention_days`.
fn schedule_toml(retention_line: &str) -> String {
    format!(
        r#"
[[schedules]]
name = "cleanup_dlq_daily"
cron = "0 0 3 * * *"
{retention_line}
"#
    )
}

#[test]
fn retention_days_zero_is_rejected_at_parse() {
    let toml_str = schedule_toml("retention_days = 0");
    let err = toml::from_str::<ApalisConfig>(&toml_str)
        .expect_err("retention_days = 0 doit refuser le démarrage (purge totale du DLQ)");
    let msg = err.to_string();
    assert!(
        msg.contains("retention_days"),
        "le message d'erreur doit désigner le champ fautif, obtenu : {msg}"
    );
}

#[test]
fn retention_days_absent_still_defaults_to_thirty() {
    // Forme de la config LIVE : aucune clé `retention_days`. Le défaut serde de 30 jours
    // doit continuer de s'appliquer — le durcissement ne change rien ici.
    let cfg: ApalisConfig =
        toml::from_str(&schedule_toml("")).expect("une config sans retention_days doit charger");
    assert_eq!(cfg.schedules.len(), 1);
    assert_eq!(cfg.schedules[0].retention_days, 30);
}

#[test]
fn retention_days_positive_is_accepted() {
    // Valeur employée par `tests/monitor_integration.rs` — doit rester acceptée.
    let cfg: ApalisConfig = toml::from_str(&schedule_toml("retention_days = 14"))
        .expect("une rétention positive doit rester acceptée");
    assert_eq!(cfg.schedules[0].retention_days, 14);
}

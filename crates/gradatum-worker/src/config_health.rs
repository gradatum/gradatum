//! Détection, journalisation et exposition des **replis de configuration** du worker.
//!
//! Le worker lit son `server.toml` section par section, et chaque section absente ou
//! malformée le fait retomber sur des valeurs par défaut **sans bloquer le boot** —
//! comportement voulu, arbitré le 2026-08-01 (`decisions/01KYYV5BHS2XDBXFF2H6Y6FWV6`).
//!
//! Le danger n'est pas le repli, c'est son **silence**. Une section mal orthographiée
//! (`[embeds]` au lieu de `[embed]`) et une section volontairement omise produisent le
//! même état final ; sans discrimination, une faute de frappe devient indiscernable d'un
//! choix d'exploitation. Précédent payé : F-120, où un puits d'événements inerte est resté
//! invisible parce que rien ne signalait sa mise en repli.
//!
//! Ce module apporte deux garanties, et rien de plus :
//!
//! 1. **Distinguer les causes** — [`FallbackCause`] sépare fichier absent, section absente
//!    et échec de désérialisation. Chaque repli est journalisé avec sa cause et l'effet
//!    concret qu'il produit ; l'échec de désérialisation, presque toujours une faute de
//!    frappe, sort en `ERROR` et non en `WARN`.
//! 2. **Rendre l'état interrogeable par une machine** — [`ConfigHealth::publish`] projette
//!    l'état de chaque section dans la jauge Prometheus `gradatum_config_degraded`, servie
//!    par le serveur `/metrics` déjà exposé par le worker. Un journal qu'il faut penser à
//!    lire n'est pas un destinataire ; une série scrutable en est un.
//!
//! Aucun repli n'est retiré et aucun boot n'est bloqué : ce module observe, il n'arbitre
//! pas.

use std::path::Path;

use figment::{
    Figment,
    providers::{Format, Toml},
};
use serde::de::DeserializeOwned;
use tracing::{error, warn};

use crate::metrics::WorkerMetrics;

/// Valeur du label `cause` employée pour une section correctement chargée.
///
/// Publier explicitement les sections saines évite de confondre « configuration
/// nominale » et « worker qui n'a jamais démarré » : dans les deux cas la série serait
/// sinon absente.
const CAUSE_HEALTHY: &str = "none";

/// Cause d'un repli sur les valeurs par défaut d'une section de configuration.
///
/// Ces trois cas produisent le même état final — les valeurs par défaut — mais n'ont ni
/// la même probabilité d'être intentionnels, ni la même gravité.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackCause {
    /// Le fichier de configuration lui-même est introuvable.
    ///
    /// Toutes les sections retombent ensemble sur leurs valeurs par défaut.
    FileMissing,
    /// Le fichier existe, mais la section n'y figure pas.
    ///
    /// Cas ambigu : souvent une omission délibérée, parfois une section mal orthographiée.
    /// Ces deux situations sont **indiscernables** depuis le fichier seul — d'où la
    /// journalisation systématique plutôt qu'un silence.
    SectionMissing,
    /// La section est présente mais sa désérialisation a échoué : type erroné, valeur hors
    /// bornes, table malformée.
    ///
    /// Cas grave. Personne n'écrit une section pour qu'elle soit ignorée : une section
    /// présente et rejetée signale presque toujours une erreur de saisie que l'exploitant
    /// croit active.
    ParseFailed,
}

impl FallbackCause {
    /// Étiquette stable servant de valeur au label Prometheus `cause`.
    ///
    /// Stable au sens contractuel : ces chaînes apparaissent dans les règles d'alerte,
    /// les modifier casserait les requêtes existantes.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FileMissing => "file_missing",
            Self::SectionMissing => "section_missing",
            Self::ParseFailed => "parse_failed",
        }
    }
}

/// État de chargement des sections de configuration consultées au boot.
///
/// Accumulé au fil des appels à [`load_section`] puis publié en une fois par
/// [`ConfigHealth::publish`]. L'ordre des entrées suit l'ordre de consultation.
///
/// La liste des sections surveillées n'est **pas** gravée dans une constante : elle se
/// construit à l'usage. Une section ajoutée au worker sans être ajoutée à une liste
/// figée serait sinon invisible, et le cardinal annoncé mentirait.
#[derive(Debug, Default)]
pub struct ConfigHealth {
    /// Une entrée par section consultée : `None` = chargée telle qu'écrite,
    /// `Some(cause)` = repli sur les valeurs par défaut.
    entries: Vec<(&'static str, Option<FallbackCause>)>,
}

impl ConfigHealth {
    /// Crée un état vide, avant toute lecture de configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre le résultat de chargement d'une section.
    ///
    /// Une section consultée deux fois est enregistrée deux fois : la publication écrase
    /// alors la série par la dernière valeur. Le worker lit chaque section une seule fois
    /// au boot, ce cas ne se présente pas en pratique.
    pub fn record(&mut self, section: &'static str, cause: Option<FallbackCause>) {
        self.entries.push((section, cause));
    }

    /// Itère sur les seules sections en repli, avec leur cause.
    pub fn degraded(&self) -> impl Iterator<Item = (&'static str, FallbackCause)> + '_ {
        self.entries
            .iter()
            .filter_map(|(section, cause)| cause.map(|c| (*section, c)))
    }

    /// Indique si au moins une section a basculé sur ses valeurs par défaut.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.degraded().next().is_some()
    }

    /// Rend les sections en repli sous la forme `section=cause`, séparées par des virgules.
    ///
    /// Destiné au champ `sections` du récapitulatif de boot. Chaîne vide si tout est sain.
    #[must_use]
    pub fn degraded_summary(&self) -> String {
        self.degraded()
            .map(|(section, cause)| format!("{section}={}", cause.label()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Publie l'état de chaque section consultée dans `gradatum_config_degraded`.
    ///
    /// Une série par section, toujours présente : `0` pour une section saine (label
    /// `cause="none"`), `1` pour une section en repli (label portant la cause). La
    /// requête d'alerte se réduit donc à `gradatum_config_degraded > 0`, et le label
    /// `cause` rend le diagnostic sans consulter le journal.
    ///
    /// À appeler **avant** le démarrage du serveur de métriques, pour que la toute
    /// première collecte voie déjà l'état réel.
    pub fn publish(&self, metrics: &WorkerMetrics) {
        for (section, cause) in &self.entries {
            match cause {
                Some(c) => metrics.set_config_degraded(section, c.label(), 1.0),
                None => metrics.set_config_degraded(section, CAUSE_HEALTHY, 0.0),
            }
        }
    }
}

/// Charge une section du TOML serveur, en retombant sur `T::default()` si elle est
/// inutilisable — repli **tracé**, jamais muet.
///
/// Remplace cinq extractions figment copiées-collées qui différaient par le nom de la
/// section et le message, et confondaient toutes « absente » et « inutilisable ».
///
/// `effect` décrit en clair ce que le repli produit du point de vue de l'exploitant
/// (« embarqueur HTTP par défaut », « métriques désactivées »…). C'est l'information qui
/// manque le plus quand on lit un journal a posteriori : la cause dit ce qui s'est passé,
/// l'effet dit ce que cela coûte.
///
/// Le boot n'est jamais interrompu : cette fonction ne renvoie pas de `Result`, par
/// construction.
pub fn load_section<T>(
    config_path: &Path,
    section: &'static str,
    effect: &str,
    health: &mut ConfigHealth,
) -> T
where
    T: DeserializeOwned + Default,
{
    if !config_path.exists() {
        health.record(section, Some(FallbackCause::FileMissing));
        warn!(
            config = %config_path.display(),
            section = section,
            cause = FallbackCause::FileMissing.label(),
            effect = effect,
            "configuration fallback — file absent, defaults applied"
        );
        return T::default();
    }

    let fig = Figment::new().merge(Toml::file(config_path));
    match fig.extract_inner::<T>(section) {
        Ok(cfg) => {
            health.record(section, None);
            cfg
        }
        Err(e) if is_section_missing(&e) => {
            health.record(section, Some(FallbackCause::SectionMissing));
            warn!(
                config = %config_path.display(),
                section = section,
                cause = FallbackCause::SectionMissing.label(),
                effect = effect,
                "configuration fallback — section absent from the file, defaults \
                 applied. Check the spelling of the section if it was meant to apply"
            );
            T::default()
        }
        Err(e) => {
            health.record(section, Some(FallbackCause::ParseFailed));
            error!(
                config = %config_path.display(),
                section = section,
                cause = FallbackCause::ParseFailed.label(),
                effect = effect,
                error = %e,
                "configuration fallback — section PRESENT but rejected, defaults \
                 applied. A section that is written then ignored is almost always a typo"
            );
            T::default()
        }
    }
}

/// Détermine si une erreur figment signale une section absente plutôt qu'une section
/// malformée.
///
/// `figment::Error` est une liste chaînée d'erreurs, parcourable par `IntoIterator` — qui
/// consomme la valeur, d'où le clone.
///
/// Le vide est traité comme **non-absent** : `Iterator::all` répond `true` sur une suite
/// vide, ce qui classerait une erreur sans détail en « section absente » et la ferait
/// taire. À information manquante, on retient l'hypothèse la plus bruyante.
fn is_section_missing(e: &figment::Error) -> bool {
    let kinds: Vec<_> = e.clone().into_iter().collect();
    !kinds.is_empty() && kinds.iter().all(figment::Error::missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Config minimale désérialisable, dotée de `Default`, pour éprouver les trois causes.
    #[derive(Debug, Default, serde::Deserialize, PartialEq)]
    struct Probe {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        port: u16,
    }

    /// Écrit un TOML temporaire et rend son handle (à garder vivant pour le chemin).
    fn toml_file(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("création du fichier temporaire");
        f.write_all(content.as_bytes())
            .expect("écriture du TOML temporaire");
        f.flush().expect("flush du TOML temporaire");
        f
    }

    #[test]
    fn section_bien_formee_est_chargee_et_marquee_saine() {
        let f = toml_file("[probe]\nenabled = true\nport = 42\n");
        let mut health = ConfigHealth::new();

        let cfg: Probe = load_section(f.path(), "probe", "effet", &mut health);

        assert_eq!(
            cfg,
            Probe {
                enabled: true,
                port: 42
            }
        );
        assert!(
            !health.is_degraded(),
            "une section valide n'est pas un repli"
        );
    }

    #[test]
    fn fichier_absent_donne_la_cause_file_missing() {
        let mut health = ConfigHealth::new();

        let _: Probe = load_section(
            Path::new("/nonexistent/gradatum/server.toml"),
            "probe",
            "effet",
            &mut health,
        );

        assert_eq!(
            health.degraded().collect::<Vec<_>>(),
            vec![("probe", FallbackCause::FileMissing)]
        );
    }

    #[test]
    fn section_absente_donne_la_cause_section_missing() {
        let f = toml_file("[autre]\nenabled = true\n");
        let mut health = ConfigHealth::new();

        let _: Probe = load_section(f.path(), "probe", "effet", &mut health);

        assert_eq!(
            health.degraded().collect::<Vec<_>>(),
            vec![("probe", FallbackCause::SectionMissing)]
        );
    }

    /// Le cas discriminant : une section présente mais dont un champ porte un type erroné
    /// ne doit PAS être confondue avec une section absente.
    #[test]
    fn section_presente_mais_malformee_donne_la_cause_parse_failed() {
        let f = toml_file("[probe]\nenabled = \"oui\"\n");
        let mut health = ConfigHealth::new();

        let _: Probe = load_section(f.path(), "probe", "effet", &mut health);

        assert_eq!(
            health.degraded().collect::<Vec<_>>(),
            vec![("probe", FallbackCause::ParseFailed)],
            "une section rejetée doit se distinguer d'une section absente"
        );
    }

    #[test]
    fn publie_zero_pour_les_saines_et_un_pour_les_degradees() {
        let metrics = WorkerMetrics::new();
        let mut health = ConfigHealth::new();
        health.record("saine", None);
        health.record("cassee", Some(FallbackCause::ParseFailed));

        health.publish(&metrics);
        let rendu = metrics.render();

        assert!(
            rendu.contains(r#"gradatum_config_degraded{cause="none",section="saine"} 0"#),
            "la série d'une section saine doit être publiée à 0 — rendu : {rendu}"
        );
        assert!(
            rendu.contains(r#"gradatum_config_degraded{cause="parse_failed",section="cassee"} 1"#),
            "la série d'une section en repli doit porter sa cause — rendu : {rendu}"
        );
    }

    #[test]
    fn le_recapitulatif_nomme_chaque_section_avec_sa_cause() {
        let mut health = ConfigHealth::new();
        health.record("embed", Some(FallbackCause::ParseFailed));
        health.record("apalis", None);
        health.record("curator", Some(FallbackCause::SectionMissing));

        assert_eq!(
            health.degraded_summary(),
            "embed=parse_failed, curator=section_missing"
        );
    }
}

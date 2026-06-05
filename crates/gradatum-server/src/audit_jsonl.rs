//! JsonlFileSink — production audit sink avec rotation daily.
//!
//! P2.0b caveat C4 : audit log JSONL mode 0640, rotation par date UTC.
//! T7 P2.0c caveat C-aud-4 : compteur atomique `dropped_total` accessible
//! depuis les fixtures de test pour vérifier les erreurs I/O.
//!
//! ## Fichiers produits
//!
//! `${base_dir}/audit.YYYY-MM-DD.jsonl` avec permissions `0640`.
//!
//! La rotation se déclenche dès que la date UTC de l'événement change par
//! rapport à la date du fichier courant. Un seul fichier par jour.
//!
//! ## Concurrence
//!
//! Le fichier courant est protégé par un `tokio::sync::Mutex`. Les écritures
//! sont séquentialisées — adapté au débit d'audit HTTP (< 10 k req/s).
//! Pour des débits supérieurs, envisager un canal + tâche dédiée.
//!
//! ## Compteur dropped_total
//!
//! Chaque appel `record` qui retourne une erreur I/O incrémente atomiquement
//! `dropped_total`. Ce compteur est lisible via `dropped_total()` pour les
//! fixtures de test et le monitoring (caveat C-aud-4).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

/// État interne du sink : fichier ouvert + date courante.
// Phase 2.1 : Inner sera utilisé quand JsonlFileSink sera câblé dans AppState.
#[allow(dead_code)]
struct Inner {
    /// Date du fichier courant au format `YYYY-MM-DD`.
    current_date: String,
    /// Handle tokio sur le fichier en cours d'écriture.
    file: tokio::fs::File,
}

/// Sink d'audit JSONL avec rotation quotidienne.
///
/// Les événements sont sérialisés en JSON sur une seule ligne, terminés par
/// `\n`, puis flushés immédiatement pour minimiser la perte en cas de crash.
///
/// La rotation se fait à la frontière de jour UTC (basée sur `event.ts`).
///
/// ## Compteur dropped_total
///
/// Incrémenté à chaque erreur I/O (disque plein, droits insuffisants, etc.).
/// Accessible via [`JsonlFileSink::dropped_total`] pour les fixtures de test
/// et le monitoring (caveat C-aud-4).
// Phase 2.1 : JsonlFileSink sera câblé dans AppState (with_audit_dir).
#[allow(dead_code)]
pub struct JsonlFileSink {
    /// Répertoire de base pour les fichiers d'audit.
    base_dir: PathBuf,
    /// Fichier courant + date, protégés par un mutex tokio.
    current: Arc<Mutex<Option<Inner>>>,
    /// Compteur atomique d'événements non persistés suite à erreur I/O.
    ///
    /// Incrémenté dans `record` sur chaque `Err`. Jamais décrémenté.
    /// Caveat C-aud-4 : accessible depuis les tests pour vérifier la saturation.
    dropped_total: Arc<AtomicU64>,
}

// Phase 2.1 : méthodes câblées dans AppState::with_audit_dir.
#[allow(dead_code)]
impl JsonlFileSink {
    /// Crée un nouveau sink qui écrira ses fichiers dans `base_dir`.
    ///
    /// Le répertoire sera créé automatiquement au premier appel à `record`.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            current: Arc::new(Mutex::new(None)),
            dropped_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Retourne le nombre cumulé d'événements non persistés suite à erreur I/O.
    ///
    /// Valeur atomique — lecture Relaxed, suffisant pour monitoring + tests.
    /// Caveat C-aud-4 : accessible depuis les fixtures de test.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }

    /// Logique interne d'enregistrement — appelée depuis `record`.
    ///
    /// Séparée pour permettre à `record` d'encapsuler le comptage des erreurs
    /// sans duplication de code (pattern Extract Inner pour le compteur C-aud-4).
    async fn record_inner(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        let today = event.ts.format("%Y-%m-%d").to_string();
        let mut guard = self.current.lock().await;

        // Rotation : premier appel ou franchissement de minuit UTC.
        let needs_rotate = guard
            .as_ref()
            .is_none_or(|inner| inner.current_date != today);

        if needs_rotate {
            let file = self.open_file_for_date(&today).await?;
            *guard = Some(Inner {
                current_date: today.clone(),
                file,
            });
        }

        // SAFETY : guard est Some après la rotation ci-dessus.
        let inner = guard
            .as_mut()
            .expect("Inner est Some — initialisé juste au-dessus");

        let line = serde_json::to_string(&event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        inner.file.write_all(line.as_bytes()).await?;
        inner.file.write_all(b"\n").await?;
        inner.file.flush().await?;

        Ok(())
    }

    /// Ouvre (ou crée) le fichier `audit.{date}.jsonl` en mode append 0640.
    async fn open_file_for_date(&self, date: &str) -> Result<tokio::fs::File, std::io::Error> {
        tokio::fs::create_dir_all(&self.base_dir).await?;
        let path = self.base_dir.join(format!("audit.{date}.jsonl"));
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(&path)
            .await
    }
}

#[async_trait]
impl AuditSink for JsonlFileSink {
    /// Enregistre `event` en JSONL dans `audit.YYYY-MM-DD.jsonl`.
    ///
    /// ## Comportement
    ///
    /// - Crée le répertoire de base si absent.
    /// - Effectue une rotation si la date UTC de l'événement diffère de la
    ///   date du fichier courant.
    /// - Chaque ligne est flushée immédiatement.
    ///
    /// ## Erreurs
    ///
    /// `std::io::Error` en cas d'échec I/O (création dir, open, write, flush).
    /// La sérialisation JSON échoue uniquement si l'événement contient des
    /// valeurs non sérialisables (ex. NaN dans `curator`) — erreur `InvalidData`.
    ///
    /// ## Compteur dropped_total
    ///
    /// Toute erreur retournée incrémente `dropped_total` (caveat C-aud-4).
    /// Cela inclut les erreurs de création de répertoire, d'ouverture de fichier,
    /// d'écriture et de flush.
    async fn record(&self, event: HttpAuditEvent) -> Result<(), std::io::Error> {
        let result = self.record_inner(event).await;
        if result.is_err() {
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("audit sink : événement non persisté — dropped_total incrémenté");
        }
        result
    }
}

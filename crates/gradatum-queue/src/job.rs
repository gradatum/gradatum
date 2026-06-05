//! Types `Job` et `JobStatus`.
//!
//! `JobStatus` est stocké en base comme texte lowercase (`pending`, `claimed`,
//! `done`, `failed`). La conversion `FromSql`/`ToSql` gère la sérialisation.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use ulid::Ulid;

/// État d'un job dans la queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// En attente de traitement.
    Pending,
    /// Claim actif — lease en cours.
    Claimed,
    /// Traitement terminé avec succès.
    Done,
    /// Traitement échoué définitivement (Phase 1 : pas de retry automatique).
    Failed,
}

impl JobStatus {
    /// Représentation textuelle stockée en base.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl ToSql for JobStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for JobStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        match s {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(FromSqlError::Other(
                format!("valeur JobStatus inconnue : {other}").into(),
            )),
        }
    }
}

/// Un job dans la queue.
///
/// `id` est un ULID monotone qui sert aussi d'ordre FIFO via `created_at`.
#[derive(Debug, Clone)]
pub struct Job {
    /// Identifiant unique du job.
    pub id: Ulid,
    /// Type de travail (ex. `embed_note`, `reindex_note`).
    pub kind: String,
    /// Payload sérialisé en JSON opaque au niveau queue.
    pub payload_json: String,
    /// État courant.
    pub status: JobStatus,
    /// Timestamp d'expiration de la lease en millisecondes Unix.
    /// `None` quand le job est `pending`, `done` ou `failed`.
    pub lease_until: Option<i64>,
    /// Timestamp de création en millisecondes Unix.
    pub created_at: i64,
    /// Timestamp de dernière mise à jour en millisecondes Unix.
    pub updated_at: i64,
    /// Nombre de tentatives de claim (incrémenté à chaque `claim_one`).
    pub attempts: u32,
    /// Dernière erreur enregistrée via `fail()`.
    pub last_error: Option<String>,
}

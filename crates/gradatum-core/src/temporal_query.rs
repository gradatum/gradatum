//! Request/response types for temporal timeline reads (`vault_timeline`).
//!
//! Consumes the `temporal_index` table. Canonical sort order:
//! `anchor_ms DESC, note_id DESC`. Validity windowing via `TimelineFilter::as_of_ms`
//! and `include_expired`.

use crate::error::{GradatumError, ValidationError};
use crate::identity::NoteId;

/// Opaque pagination cursor: `(anchor_ms, note_id)` of the last item returned.
///
/// URL-safe encoding without extra dependencies: `"{anchor_ms}.{note_id}"`. The
/// `note_id` (ULID) is alphanumeric; `anchor_ms` is a decimal integer; the `.`
/// separator is unambiguous. The next page contains rows strictly "before" this
/// cursor in `anchor_ms DESC, note_id DESC` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineCursor {
    /// Temporal anchor (epoch ms) of the last item of the previous page.
    pub anchor_ms: i64,
    /// ULID of the last item of the previous page (tiebreaker).
    pub note_id: String,
}

impl TimelineCursor {
    /// Encodes the cursor as a URL-safe opaque string `"{anchor_ms}.{note_id}"`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}.{}", self.anchor_ms, self.note_id)
    }

    /// Decodes an opaque cursor string.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Validation(ValidationError::InvalidInput)` if the
    /// string is malformed (missing separator, non-numeric `anchor_ms`, empty or
    /// oversized `note_id` > 32 chars).
    pub fn decode(s: &str) -> Result<Self, GradatumError> {
        let (ms, id) = s.split_once('.').ok_or_else(|| {
            GradatumError::Validation(ValidationError::InvalidInput(
                "cursor malformé: séparateur absent".into(),
            ))
        })?;
        let anchor_ms = ms.parse::<i64>().map_err(|_| {
            GradatumError::Validation(ValidationError::InvalidInput(
                "cursor malformé: anchor_ms".into(),
            ))
        })?;
        if id.is_empty() {
            return Err(GradatumError::Validation(ValidationError::InvalidInput(
                "cursor malformé: note_id vide".into(),
            )));
        }
        // V4 — borne défensive : un ULID fait 26 chars, on tolère 32. Au-delà,
        // c'est un cursor forgé (jamais émis par encode()) → rejet.
        if id.len() > 32 {
            return Err(GradatumError::Validation(ValidationError::InvalidInput(
                "cursor malformé: note_id surdimensionné".into(),
            )));
        }
        Ok(Self {
            anchor_ms,
            note_id: id.to_string(),
        })
    }
}

/// Server-internal filters for `IndexStore::timeline`.
///
/// `limit` is already clamped to `[1, 200]` by the handler before reaching the
/// index; the index implementation re-clamps as a defence-in-depth measure.
///
/// ## Validity semantics
///
/// The pair `(as_of_ms, include_expired)` controls temporal filtering:
///
/// | `as_of_ms` | `include_expired` | Behaviour |
/// |---|---|---|
/// | `None` | `false` (default) | No validity filter — all notes returned |
/// | `None` | `true` | No validity filter (`include_expired` has no effect without a reference time) |
/// | `Some(t)` | `false` (default) | Strict filter: `anchor_ms ≤ t AND (valid_until IS NULL OR t < valid_until)` |
/// | `Some(t)` | `true` | Historical: `anchor_ms ≤ t` (notes born before T, **including those expired at T**) |
///
/// **`include_expired=true` use case**: historical query — "which notes existed at time T,
/// whether still valid or not?" Useful for debugging or reconstructing past state.
#[derive(Debug, Clone)]
pub struct TimelineFilter {
    /// Filter `doc_kind IN (...)`. `None` or empty = all (`Static`/`Event`).
    pub doc_kind: Option<Vec<String>>,
    /// Inclusive lower bound on `anchor_ms`.
    pub from_ms: Option<i64>,
    /// Inclusive upper bound on `anchor_ms`.
    pub to_ms: Option<i64>,
    /// Maximum number of rows (clamped to `[1,200]`).
    pub limit: usize,
    /// Pagination cursor (`None` = first page).
    pub cursor: Option<TimelineCursor>,
    /// "As-of" reference instant for validity filtering (epoch ms UTC).
    ///
    /// `Some(t)` activates validity filtering (see semantics table above).
    /// `None` disables validity filtering.
    pub as_of_ms: Option<i64>,
    /// Controls inclusion of expired notes **when `as_of_ms = Some(t)`**.
    ///
    /// - `false` (default): excludes notes whose `valid_until_ms < t` (strict filter).
    /// - `true`: includes notes expired at T (historical filter — `anchor_ms ≤ t` only).
    ///
    /// Without `as_of_ms`, this flag has no effect (no validity filtering is active).
    pub include_expired: bool,
}

impl Default for TimelineFilter {
    fn default() -> Self {
        Self {
            doc_kind: None,
            from_ms: None,
            to_ms: None,
            limit: 50,
            cursor: None,
            as_of_ms: None,
            include_expired: false,
        }
    }
}

/// A single timeline row (join of `temporal_index` × `notes`).
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRow {
    /// ULID of the note.
    pub note_id: NoteId,
    /// Temporal anchor (epoch ms).
    pub anchor_ms: i64,
    /// Anchor source (`created`/`occurred_at`/`event-date`/`valid_from`).
    pub anchor_src: String,
    /// CoALA axis (`Static`/`Event`).
    pub doc_kind: String,
    /// H1 title of the note (`None` if absent).
    pub title: Option<String>,
}

/// Parses a temporal string as a UTC epoch in milliseconds.
///
/// This is the **canonical parser** shared between the validation layer (server)
/// and the resolution layer (worker). Both sides MUST use this function to ensure
/// that a value accepted by the server (HTTP 202) is also accepted by the worker
/// (otherwise an invalid string accepted server-side would silently fall back to
/// `anchor_src=Created` in the worker — breaking the semantic guarantee).
///
/// ## Accepted formats
///
/// - ISO 8601 / RFC 3339 with time: `"2026-01-15T10:00:00Z"`
/// - Date-only YYYY-MM-DD → start of day UTC: `"2026-01-15"`
///
/// ## Returns
///
/// `Some(epoch_ms)` on success, `None` if the string is malformed.
///
/// # Side effects
///
/// None. Pure function.
#[must_use]
pub fn parse_temporal_str_as_ms(s: &str) -> Option<i64> {
    // Attempt RFC 3339 (with time)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // Attempt date-only YYYY-MM-DD → start of day UTC
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        use chrono::TimeZone as _;
        let dt = chrono::Utc.from_utc_datetime(
            &d.and_hms_opt(0, 0, 0)
                .expect("hms(0,0,0) est un horaire valide — ne peut pas échouer"),
        );
        return Some(dt.timestamp_millis());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tests parse_temporal_str_as_ms (SSOT parseur) ────────────────────────

    /// ISO 8601 complet (RFC 3339) → epoch ms correct.
    #[test]
    fn parse_temporal_str_as_ms_rfc3339_returns_ms() {
        let ms = parse_temporal_str_as_ms("2026-01-15T00:00:00Z");
        assert!(ms.is_some(), "RFC3339 doit être parsé");
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
            .expect("parsing ref")
            .timestamp_millis();
        assert_eq!(ms.unwrap(), expected);
    }

    /// YYYY-MM-DD → début du jour UTC en ms.
    #[test]
    fn parse_temporal_str_as_ms_date_only_returns_start_of_day_ms() {
        let ms = parse_temporal_str_as_ms("2026-01-15");
        assert!(ms.is_some(), "YYYY-MM-DD doit être parsé");
        // 2026-01-15T00:00:00Z
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
            .expect("parsing ref")
            .timestamp_millis();
        assert_eq!(ms.unwrap(), expected);
    }

    /// Chaîne invalide → None (pas de panic).
    #[test]
    fn parse_temporal_str_as_ms_invalid_returns_none() {
        assert_eq!(parse_temporal_str_as_ms("pas-une-date"), None);
        assert_eq!(parse_temporal_str_as_ms(""), None);
        assert_eq!(parse_temporal_str_as_ms("2026-13-45"), None);
    }

    #[test]
    fn cursor_roundtrip() {
        let c = TimelineCursor {
            anchor_ms: 1_734_000_000_000,
            note_id: "01HZ".to_string(),
        };
        let encoded = c.encode();
        assert_eq!(encoded, "1734000000000.01HZ");
        assert_eq!(TimelineCursor::decode(&encoded).unwrap(), c);
    }

    #[test]
    fn cursor_decode_rejects_missing_separator() {
        assert!(TimelineCursor::decode("1734000000000").is_err());
    }

    #[test]
    fn cursor_decode_rejects_bad_ms() {
        assert!(TimelineCursor::decode("notanumber.01HZ").is_err());
    }

    #[test]
    fn cursor_decode_rejects_empty_id() {
        assert!(TimelineCursor::decode("123.").is_err());
    }

    /// V4 — un note_id surdimensionné (> 32 chars, ULID = 26) est rejeté :
    /// borne défensive contre un cursor forgé qui gonflerait les binds SQL.
    #[test]
    fn cursor_decode_rejects_oversized_id() {
        let oversized = "0".repeat(33);
        assert!(TimelineCursor::decode(&format!("123.{oversized}")).is_err());
        // 32 chars exactement reste accepté (tolérance au-dessus d'un ULID).
        assert!(TimelineCursor::decode(&format!("123.{}", "0".repeat(32))).is_ok());
    }
}

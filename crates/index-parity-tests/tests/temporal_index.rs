//! Parité backend : ancre temporelle (`write_temporal_entry`).
//!
//! Invariant : `write_temporal_entry` est idempotent (`INSERT OR REPLACE` sur la
//! clé primaire `note_id`) et n'erre jamais sur réécriture. Méthode promue en trait
//! (W1) — testée ici sur le type effacé.

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::{AnchorSrc, Index, TemporalEntry};
use gradatum_core::scope::VaultId;
use gradatum_core::temporal_query::{TimelineCursor, TimelineFilter};
use ulid::Ulid;

fn temporal_for(note_id: &NoteId, anchor_ms: i64) -> TemporalEntry {
    TemporalEntry {
        note_id: note_id.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    }
}

#[tokio::test]
async fn write_temporal_entry_succeeds() {
    let idx = make_index().await;
    let note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "note temporelle",
    );
    idx.write_note(&note).await.expect("write note");

    idx.write_temporal_entry(&temporal_for(&note.id, 1_700_000_000_000))
        .await
        .unwrap_or_else(|e| panic!("write_temporal_entry ({}) : {e}", common::backend_label()));
}

#[tokio::test]
async fn write_temporal_entry_is_idempotent_on_replace() {
    let idx = make_index().await;
    let note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "note temporelle",
    );
    idx.write_note(&note).await.expect("write note");

    // Première écriture puis réécriture avec une ancre différente — INSERT OR REPLACE
    // ne doit jamais erreur (idempotence sur la clé note_id).
    idx.write_temporal_entry(&temporal_for(&note.id, 1_700_000_000_000))
        .await
        .expect("write 1");
    idx.write_temporal_entry(&temporal_for(&note.id, 1_800_000_000_000))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "réécriture temporelle doit être idempotente ({}) : {e}",
                common::backend_label()
            )
        });
}

// ── Parité timeline read (F-55 zone D) ─────────────────────────────────────────
// ULID valides 26 chars, ordre lexico A < B < C (dernier char).
const A_ULID: &str = "01HQ0000000000000000000000";
const B_ULID: &str = "01HQ0000000000000000000001";
const C_ULID: &str = "01HQ0000000000000000000002";

/// Seed une note (ULID imposé) + son entrée temporelle `(anchor_ms, doc_kind)`.
async fn seed_timeline(
    idx: &std::sync::Arc<dyn Index>,
    ulid: &str,
    anchor_ms: i64,
    doc_kind: &str,
) {
    seed_timeline_with_valid_until(idx, ulid, anchor_ms, doc_kind, None).await;
}

/// Seed une note (ULID imposé) + son entrée temporelle avec `valid_until_ms` optionnel.
async fn seed_timeline_with_valid_until(
    idx: &std::sync::Arc<dyn Index>,
    ulid: &str,
    anchor_ms: i64,
    doc_kind: &str,
    valid_until_ms: Option<i64>,
) {
    let id = NoteId(Ulid::from_string(ulid).expect("ULID 26 chars valide"));
    let note = make_note_with_id(id, minimal_frontmatter("main"), "note timeline parity");
    idx.write_note(&note).await.expect("write note timeline");
    let entry = TemporalEntry {
        note_id: ulid.to_string(),
        vault_id: "main".to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: doc_kind.to_string(),
        valid_until_ms,
    };
    idx.write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry timeline parity");
}

#[tokio::test]
async fn timeline_orders_filters_paginates_parity() {
    let idx = make_index().await; // Arc<dyn Index>
    seed_timeline(&idx, A_ULID, 1000, "Event").await;
    seed_timeline(&idx, B_ULID, 2000, "Event").await;
    seed_timeline(&idx, C_ULID, 3000, "Static").await;

    // Ordre DESC,DESC
    let all = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("timeline ({}) : {e}", common::backend_label()));
    let ids: Vec<String> = all.iter().map(|r| r.note_id.0.to_string()).collect();
    assert_eq!(ids, vec![C_ULID, B_ULID, A_ULID]);

    // Filtre doc_kind
    let events = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                doc_kind: Some(vec!["Event".into()]),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 2);

    // Pagination cursor
    let p1 = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cur = TimelineCursor {
        anchor_ms: p1[1].anchor_ms,
        note_id: p1[1].note_id.0.to_string(),
    };
    let p2 = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                limit: 2,
                cursor: Some(cur),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        p2.iter()
            .map(|r| r.note_id.0.to_string())
            .collect::<Vec<_>>(),
        vec![A_ULID]
    );
}

// ── Parité as_of validité (v0.5.1) ─────────────────────────────────────────────
// ULID valide distinct : alphabet Crockford Base32 (0-9 A-Z sauf I, L, O, U).
const V_ULID: &str = "01HV0000000000000000000001";

/// Parity cas d — as_of == valid_until → exclue (borne exclusive).
///
/// Invariant : `t < valid_until_ms` est exclusif. Un backend qui utilisait `<=`
/// produirait un faux positif visible ici.
#[tokio::test]
async fn timeline_parity_as_of_equal_valid_until_excluded() {
    let idx = make_index().await;
    // anchor=1000, valid_until=5000
    seed_timeline_with_valid_until(&idx, V_ULID, 1_000, "Event", Some(5_000)).await;

    // as_of == valid_until = 5000 → exclusif → exclue
    let rows = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                as_of_ms: Some(5_000),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("timeline parity as_of ({}) : {e}", common::backend_label()));
    let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
    assert!(
        !ids.contains(&V_ULID.to_string()),
        "parity cas d : as_of == valid_until → borne exclusive → exclue ({})",
        common::backend_label()
    );
}

/// Parity cas e — as_of == anchor_ms → visible (borne incluse).
///
/// Invariant : `anchor_ms <= t` est inclusif. Un backend qui utilisait `<`
/// produirait un faux négatif visible ici.
#[tokio::test]
async fn timeline_parity_as_of_equal_anchor_included() {
    let idx = make_index().await;
    // anchor=2000, valid_until=8000
    seed_timeline_with_valid_until(&idx, V_ULID, 2_000, "Event", Some(8_000)).await;

    // as_of == anchor = 2000 → inclusif → visible
    let rows = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                as_of_ms: Some(2_000),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("timeline parity as_of ({}) : {e}", common::backend_label()));
    let ids: Vec<String> = rows.iter().map(|r| r.note_id.0.to_string()).collect();
    assert!(
        ids.contains(&V_ULID.to_string()),
        "parity cas e : as_of == anchor → borne incluse → visible ({})",
        common::backend_label()
    );
}

/// Parity include_expired — {as_of=t, include_expired=true} montre une note expirée à t.
///
/// Invariant : la clause `valid_until` est retirée quand `include_expired=true`.
/// Un backend qui ignorerait ce flag raterait les notes expirées dans les requêtes historiques.
#[tokio::test]
async fn timeline_parity_as_of_include_expired_shows_expired() {
    let idx = make_index().await;
    // anchor=1000, valid_until=2000 → expirée à t=3000
    seed_timeline_with_valid_until(&idx, V_ULID, 1_000, "Event", Some(2_000)).await;

    // include_expired=false : exclue
    let rows_strict = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                as_of_ms: Some(3_000),
                include_expired: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !rows_strict
            .iter()
            .any(|r| r.note_id.0.to_string() == V_ULID),
        "parity include_expired précondition : note expirée exclue avec include_expired=false ({})",
        common::backend_label()
    );

    // include_expired=true : visible
    let rows_hist = idx
        .timeline(
            &VaultId("main".into()),
            &TimelineFilter {
                as_of_ms: Some(3_000),
                include_expired: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        rows_hist.iter().any(|r| r.note_id.0.to_string() == V_ULID),
        "parity include_expired : note expirée visible avec include_expired=true ({})",
        common::backend_label()
    );
}

//! Tests de la state machine NoteStatus.

use gradatum_core::config::EmbedConfig;
use gradatum_core::status::NoteStatus;

/// Matrice complète des transitions autorisées et interdites.
#[test]
fn full_state_machine_matrix() {
    use NoteStatus::*;

    // Transitions autorisées.
    let allowed: &[(NoteStatus, NoteStatus)] = &[
        (Draft, PendingReview),
        (Draft, Garbage),
        (PendingReview, Live),
        (PendingReview, Garbage),
        (PendingReview, Staging),
        (Staging, Live),
        (Staging, Garbage),
        (Live, Deprecated),
        (Live, Garbage),
        (Deprecated, Live), // restore
        (Garbage, Live),    // restore avant cleanup async
    ];

    for &(from, to) in allowed {
        assert!(
            from.can_transition_to(to),
            "{from:?} → {to:?} devrait être autorisée",
        );
    }

    // Transitions interdites (sample représentatif).
    assert!(
        !Draft.can_transition_to(Live),
        "Draft → Live interdite (skip PendingReview/Staging)"
    );
    assert!(
        !Draft.can_transition_to(Deprecated),
        "Draft → Deprecated interdite"
    );
    assert!(
        !Draft.can_transition_to(Staging),
        "Draft → Staging interdite (doit passer par PendingReview d'abord)"
    );
    assert!(
        !Garbage.can_transition_to(Deprecated),
        "Garbage → Deprecated interdite"
    );
    assert!(
        !Garbage.can_transition_to(Draft),
        "Garbage → Draft interdite"
    );
    assert!(!Live.can_transition_to(Draft), "Live → Draft interdite");
    assert!(
        !Deprecated.can_transition_to(Garbage),
        "Deprecated → Garbage interdite"
    );
}

/// Valeurs par défaut d'embeddabilité — workflow-aware β.
#[test]
fn is_embeddable_default_beta() {
    use NoteStatus::*;

    // Embeddables par défaut : review-or-better.
    assert!(Live.is_embeddable_default(), "Live doit être embeddable");
    assert!(
        PendingReview.is_embeddable_default(),
        "PendingReview doit être embeddable (curator compare sémantique)"
    );
    assert!(
        Staging.is_embeddable_default(),
        "Staging doit être embeddable"
    );

    // Non-embeddables par défaut.
    assert!(
        !Draft.is_embeddable_default(),
        "Draft non-embeddable (pas d'engagement)"
    );
    assert!(
        !Deprecated.is_embeddable_default(),
        "Deprecated non-embeddable (sortant)"
    );
    assert!(
        !Garbage.is_embeddable_default(),
        "Garbage non-embeddable (sortant)"
    );
}

/// Config override strict : seul Live embeddable.
#[test]
fn is_embeddable_respects_config_override() {
    let cfg_strict = EmbedConfig {
        embeddable_status: Some(vec!["live".into()]),
        ..Default::default()
    };

    assert!(
        NoteStatus::Live.is_embeddable(&cfg_strict),
        "Live embeddable avec override strict"
    );
    assert!(
        !NoteStatus::PendingReview.is_embeddable(&cfg_strict),
        "PendingReview non-embeddable avec override strict"
    );
    assert!(
        !NoteStatus::Staging.is_embeddable(&cfg_strict),
        "Staging non-embeddable avec override strict"
    );
    assert!(
        !NoteStatus::Draft.is_embeddable(&cfg_strict),
        "Draft non-embeddable avec override strict"
    );
}

/// Config None → délègue à is_embeddable_default().
#[test]
fn is_embeddable_falls_back_on_default_when_config_none() {
    let cfg_none = EmbedConfig::default();

    // Doit se comporter comme is_embeddable_default().
    assert!(
        NoteStatus::PendingReview.is_embeddable(&cfg_none),
        "PendingReview embeddable avec config None"
    );
    assert!(
        NoteStatus::Live.is_embeddable(&cfg_none),
        "Live embeddable avec config None"
    );
    assert!(
        !NoteStatus::Draft.is_embeddable(&cfg_none),
        "Draft non-embeddable avec config None"
    );
}

/// Config override multiple statuts.
#[test]
fn is_embeddable_config_multi_statuses() {
    let cfg = EmbedConfig {
        embeddable_status: Some(vec!["live".into(), "staging".into()]),
        ..Default::default()
    };

    assert!(NoteStatus::Live.is_embeddable(&cfg));
    assert!(NoteStatus::Staging.is_embeddable(&cfg));
    assert!(!NoteStatus::PendingReview.is_embeddable(&cfg));
    assert!(!NoteStatus::Draft.is_embeddable(&cfg));
}

/// is_visible_default : seul Live visible.
#[test]
fn is_visible_default_only_live() {
    use NoteStatus::*;

    assert!(Live.is_visible_default());
    assert!(!Draft.is_visible_default());
    assert!(!Staging.is_visible_default());
    assert!(!PendingReview.is_visible_default());
    assert!(!Deprecated.is_visible_default());
    assert!(!Garbage.is_visible_default());
}

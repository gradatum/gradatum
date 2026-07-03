use gradatum_worker::quality_score::{
    QualityInputs, centroid, entity_orphan_penalty, num_coherence_penalty, score_quality,
};

#[test]
fn centroid_averages_componentwise() {
    let c = centroid(&[vec![1.0, 0.0], vec![3.0, 2.0]]);
    assert_eq!(c, vec![2.0, 1.0]);
}

#[test]
fn centroid_empty_is_empty() {
    assert!(centroid(&[]).is_empty());
}

#[test]
fn num_penalty_all_grounded_is_one() {
    let p = num_coherence_penalty(
        "total is 42 out of 100",
        &["value 42".into(), "base 100".into()],
    );
    assert!((p - 1.0).abs() < 1e-6);
}

#[test]
fn num_penalty_orphan_number_penalizes() {
    // 999 appears in no source
    let p = num_coherence_penalty("total is 999", &["value 42".into()]);
    assert!((0.5..1.0).contains(&p));
}

#[test]
fn entity_penalty_orphan_entity_penalizes() {
    // "Zorglub" absent from sources
    let p = entity_orphan_penalty(
        "according to Zorglub and Paris",
        &["report from Paris".into()],
    );
    assert!((0.5..1.0).contains(&p));
}

#[test]
fn entity_penalty_all_grounded_is_one() {
    let p = entity_orphan_penalty("according to Paris", &["report from paris".into()]);
    assert!((p - 1.0).abs() < 1e-6);
}

#[test]
fn score_quality_composes_factors() {
    let inp = QualityInputs {
        synth_embedding: &[1.0, 0.0],
        source_centroid: &[1.0, 0.0], // cosine = 1.0
        synth_body: "value 42",
        source_texts: &["value 42".to_string()],
        f17_sources: 1.0,
        f47_sources: 0.6,
    };
    let q = score_quality(&inp);
    assert!((q.grounding - 1.0).abs() < 1e-6);
    assert!((q.num_penalty - 1.0).abs() < 1e-6);
    // score = grounding(1) * f17(1) * f47(0.6) * num(1) * entity(1) = 0.6
    assert!((q.score - 0.6).abs() < 1e-6);
}

#[test]
fn score_quality_low_grounding_lowers_score() {
    let inp = QualityInputs {
        synth_embedding: &[0.0, 1.0],
        source_centroid: &[1.0, 0.0], // cosine = 0.0
        synth_body: "x",
        source_texts: &["y".to_string()],
        f17_sources: 1.0,
        f47_sources: 1.0,
    };
    let q = score_quality(&inp);
    assert!(q.score < 0.1);
}

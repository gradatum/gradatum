//! The personal-data detection pass, at the single insertion point.
//!
//! ## Contract
//!
//! Every note write funnels through [`write_note_inner`](crate::lifecycle), which
//! calls [`pass`] once. The pass is consulted — never obeyed:
//!
//! - **Two hard guards come first**, and neither can be switched off by
//!   configuration: a `code-*` write namespace and the `identity` section are
//!   never submitted to a detector. They are evaluated on the **resolved** write
//!   namespace, never on the incoming frontmatter, so a note with an empty
//!   `vault_id` written into a `code-*` vault is still skipped.
//! - **No configured detector means no pass.** A detector that declares no
//!   identity is the pass-through default: it is not called, no block is recorded
//!   and no tag is applied. The source of that answer is the detector, so mounting
//!   the pass-through explicitly is the same as never mounting one.
//! - **The surface is derived, never supplied.** No signature is widened and no
//!   caller passes a [`Surface`]: it is read from the section already carried by
//!   the frontmatter. The budget follows from the vault's own
//!   [`PrivacyBudgets`], so a request body cannot narrow either one.
//! - **A failure never blocks the write.** A detector error is a typed
//!   [`PiiDetectError`] handed back to the caller, which logs it and carries on:
//!   the note is persisted unchanged. The error is logged without the body and
//!   without any detected value.
//! - **Under the no-op detector nothing changes.** The default detector reports
//!   nothing, so a fresh note is written byte-identically to before this frontier
//!   existed.
//!
//! ## Windowing and the boundary count
//!
//! A body larger than the per-call ceiling is cut into overlapping windows, each
//! submitted on its own, and every span a detector returns is rebased onto the body
//! before validation. Fusion runs across windows, so an entity sitting in the
//! overlap is one span, not two.
//!
//! [`validate_and_merge`] sees a body and spans, not a window layout, so it leaves
//! [`MergeStats::touching_window_edge`] at zero. This module is the only holder of
//! the [`WindowPlan`], and therefore the single place that fills it: a span whose
//! range comes within one byte of an interior cut is counted, once per span. A body
//! that fits in one window has no interior cut and reports zero. The count is the
//! only observable signal that the overlap may be shorter than an entity.

use std::sync::Arc;
use std::time::Duration;

use gradatum_core::pii::{
    Budget, DetectInput, Detection, PiiDetectError, PiiDetector, PiiSpan, PrivacyBudgets, Surface,
};
use gradatum_core::section::Section;
use gradatum_privacy::{MergeStats, WindowPlan, rebase, validate_and_merge};

/// Prefix of a write namespace that is never submitted to a detector.
///
/// A `code-*` vault holds source code and its companion artefacts: a personal-data
/// frontier there would flag identifiers, fixtures and addresses that are the
/// material itself. The same lexical test guards vault provisioning in the index
/// layer; this one guards the write path.
pub(crate) const CODE_VAULT_PREFIX: &str = "code-";

/// The write surface a section is written on.
///
/// `Messaging` is written by `message_send` alone, on a synchronous HTTP request
/// that pays the detection on the wire; every other section is written through the
/// asynchronous paths (curate, forget, distill, restore). The mapping is derived
/// from the section, never declared per call site.
pub(crate) fn surface_for(section: Section) -> Surface {
    match section {
        Section::Messaging => Surface::Sync,
        _ => Surface::Async,
    }
}

/// Hard guard 0 — is this write excluded from detection altogether?
///
/// `vault_id` must be the **resolved** write namespace: a caller-supplied empty
/// `vault_id` is filled from the vault before this test, otherwise a note written
/// into a `code-*` vault would slip through on an empty field.
pub(crate) fn is_excluded(vault_id: &str, section: Section) -> bool {
    vault_id.starts_with(CODE_VAULT_PREFIX) || section == Section::Identity
}

/// Outcome of a detection pass that completed.
pub(crate) struct PrivacyScan {
    /// Validated, fused spans, in body order. Nothing is applied to the note yet:
    /// the pass only reports what it found.
    pub(crate) spans: Vec<PiiSpan>,
    /// Bookkeeping of the validation and fusion, boundary count included.
    pub(crate) stats: MergeStats,
    /// Number of windows the body was cut into.
    pub(crate) windows: usize,
    /// Detector labels no [`PiiCategory`](gradatum_core::pii::PiiCategory) matched,
    /// summed over the windows.
    ///
    /// The detector reports a **count**, never the labels themselves — the core
    /// contract fixed that, so an observer can report how many labels were dropped
    /// and cannot report which. Carried out of the pass rather than dropped at the
    /// window, so the number reaches the frontier's observer instead of dying in the
    /// loop.
    pub(crate) unmapped_labels: u64,
}

/// Runs the detection pass for one write, if the write is not excluded.
///
/// Returns `Ok(None)` when hard guard 0 excludes the write — no detector is called
/// at all — or when the mounted detector declares no identity: no detection is
/// configured, so there is no pass to run, no block to record and no tag to apply.
/// A block the note already carried is left alone by that path, never removed.
///
/// Returns `Err` when the detector failed; the caller records the failure on the
/// note with the note left unchanged.
pub(crate) async fn pass(
    detector: &Arc<dyn PiiDetector>,
    vault_id: &str,
    section: Section,
    body: &str,
    budgets: &PrivacyBudgets,
) -> Result<Option<PrivacyScan>, PiiDetectError> {
    if is_excluded(vault_id, section) {
        return Ok(None);
    }
    // Step 1 of the insertion point: an unconfigured detector is a pass-through, not
    // a failed detector. It is read from the detector itself — never from the fact
    // that one was injected — so mounting the pass-through explicitly changes
    // nothing, and a future real detector cannot be mistaken for an absent one.
    if detector.identity().is_none() {
        return Ok(None);
    }
    let scan = scan(detector, body, surface_for(section), budgets).await?;
    Ok(Some(scan))
}

/// Cuts `body`, submits every window, rebases, validates and fuses.
///
/// # Errors
///
/// Propagates the first window's error: a body that is not analysed in full is
/// never reported as a clean scan. The remaining windows are not submitted — the
/// caller's contract on error is to mark the note unscanned, which is the same
/// answer for one lost window as for all of them.
pub(crate) async fn scan(
    detector: &Arc<dyn PiiDetector>,
    body: &str,
    surface: Surface,
    budgets: &PrivacyBudgets,
) -> Result<PrivacyScan, PiiDetectError> {
    let plan = WindowPlan::new(budgets.max_input_bytes, budgets.window_overlap_bytes);
    let windows = plan.windows(body);

    // A degenerate plan cuts the body into nothing: no window is ever submitted, so
    // the pass has scanned none of it. Reporting that as a clean scan of an unread
    // body is the one answer that must not be given.
    if windows.is_empty() {
        return Err(PiiDetectError::InputTooLarge {
            len: body.len(),
            budget: 0,
        });
    }

    let budget = budgets.for_surface(surface, body.len());
    let mut collected: Vec<PiiSpan> = Vec::new();
    let mut unmapped_labels: u64 = 0;
    for (start, window) in &windows {
        let detection = detect_window(detector, window, surface, budget).await?;
        let mut spans = detection.spans;
        // A detector answers in the coordinates of the window it was handed. Without
        // this shift, a window-relative offset that happens to fall inside the body
        // is a wrong offset that validation cannot tell from a right one.
        rebase(&mut spans, *start);
        collected.extend(spans);
        unmapped_labels = unmapped_labels.saturating_add(detection.unmapped_labels);
    }

    let (spans, mut stats) = validate_and_merge(body, collected);

    // Ruling G6 — `validate_and_merge` knows no window layout, so the interior
    // boundary count is filled here, by the only holder of the plan, once.
    let intercuts: Vec<usize> = windows
        .iter()
        .take(windows.len() - 1)
        .map(|(start, text)| start + text.len())
        .collect();
    stats.touching_window_edge = spans
        .iter()
        .filter(|span| intercuts.iter().any(|cut| touches_cut(span, *cut)))
        .count();

    Ok(PrivacyScan {
        spans,
        stats,
        windows: windows.len(),
        unmapped_labels,
    })
}

/// Submits one window to the detector, under the budget's ceiling and delay.
///
/// The ceiling is enforced **before** the call. A detector that ignores
/// `input.budget` — the regex engine does, by design — would otherwise be handed an
/// oversized window and answer as if it had scanned the whole of it.
///
/// The detector is synchronous and can take hundreds of milliseconds, so it runs on
/// a blocking thread: no lock is held across an `await`, and the async executor is
/// not occupied by a detector that is merely slow.
async fn detect_window(
    detector: &Arc<dyn PiiDetector>,
    text: &str,
    surface: Surface,
    budget: Budget,
) -> Result<Detection, PiiDetectError> {
    if text.len() > budget.max_input_bytes {
        return Err(PiiDetectError::InputTooLarge {
            len: text.len(),
            budget: budget.max_input_bytes,
        });
    }

    // `spawn_blocking` needs an owned closure, hence the owned window text: the
    // detector borrows it for the length of one synchronous call.
    let owned = text.to_owned();
    let detector = Arc::clone(detector);
    let call = tokio::task::spawn_blocking(move || {
        detector.detect(&DetectInput::new(&owned, surface, budget))
    });

    match tokio::time::timeout(Duration::from_millis(budget.timeout_ms), call).await {
        // The blocking task cannot be cancelled; it is abandoned and its answer
        // dropped. The write does not wait for it.
        Err(_elapsed) => Err(PiiDetectError::Timeout {
            timeout_ms: budget.timeout_ms,
        }),
        // A reason string, never the join error's message: a detector that panicked
        // may have put body content in it, and this error reaches a log line.
        Ok(Err(join)) => Err(PiiDetectError::Unavailable {
            reason: if join.is_panic() {
                "detector task panicked".to_owned()
            } else {
                "detector task cancelled".to_owned()
            },
        }),
        Ok(Ok(result)) => result,
    }
}

/// Does `span` come within one byte of the cut at `cut`?
///
/// Either end landing on the cut, or one byte away from it, counts: the span is
/// close enough that the overlap, not the detector, decided how much of it was
/// visible. Saturating arithmetic keeps a hostile span from overflowing.
fn touches_cut(span: &PiiSpan, cut: usize) -> bool {
    let end = span.start.saturating_add(span.len);
    span.start <= cut.saturating_add(1) && end.saturating_add(1) >= cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::pii::{Detection, PiiCategory};

    /// Finds a literal marker and reports it in the window's own coordinates —
    /// which is what a window-aware detector does, and the reason `rebase` exists.
    struct MarkerDetector {
        marker: &'static str,
    }

    impl PiiDetector for MarkerDetector {
        fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
            let spans = input
                .text
                .find(self.marker)
                .map(|start| {
                    vec![PiiSpan::new(
                        start,
                        self.marker.len(),
                        PiiCategory::Secret,
                        0.5,
                    )]
                })
                .unwrap_or_default();
            Ok(Detection::new(spans, 0))
        }
    }

    /// Ignores `input.budget` entirely — the behaviour the wrapper must not trust.
    struct BudgetIgnoringDetector;

    impl PiiDetector for BudgetIgnoringDetector {
        fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
            Ok(Detection::new(Vec::new(), 0))
        }
    }

    struct SlowDetector;

    impl PiiDetector for SlowDetector {
        fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
            std::thread::sleep(Duration::from_millis(2_000));
            Ok(Detection::new(Vec::new(), 0))
        }
    }

    fn detector<T: PiiDetector + 'static>(d: T) -> Arc<dyn PiiDetector> {
        Arc::new(d)
    }

    #[test]
    fn a_code_star_namespace_is_excluded_whatever_the_section() {
        for section in Section::ALL {
            assert!(
                is_excluded("code-demo", section),
                "a `code-*` write namespace is never scanned ({section:?})"
            );
        }
    }

    #[test]
    fn an_identity_section_is_excluded_whatever_the_namespace() {
        assert!(is_excluded("main", Section::Identity));
        assert!(
            !is_excluded("main", Section::Decisions),
            "the guard is not a general refusal"
        );
    }

    #[test]
    fn only_messaging_is_written_on_the_synchronous_surface() {
        for section in Section::ALL {
            let expected = if section == Section::Messaging {
                Surface::Sync
            } else {
                Surface::Async
            };
            assert_eq!(surface_for(section), expected, "section {section:?}");
        }
    }

    #[tokio::test]
    async fn an_input_over_the_byte_ceiling_is_refused_before_the_detector_sees_it() {
        // The detector ignores its budget: the wrapper must refuse on its behalf,
        // otherwise an oversized window is answered as if it had been scanned.
        let budget = Budget::new(1_000, 4);
        let err = detect_window(
            &detector(BudgetIgnoringDetector),
            "0123456789",
            Surface::Async,
            budget,
        )
        .await
        .expect_err("a window past the ceiling is refused");
        assert!(
            matches!(err, PiiDetectError::InputTooLarge { len: 10, budget: 4 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_detector_that_outlives_its_budget_times_out() {
        let budget = Budget::new(20, 1_024);
        let err = detect_window(&detector(SlowDetector), "body", Surface::Async, budget)
            .await
            .expect_err("a detector past its delay is a typed timeout");
        assert!(
            matches!(err, PiiDetectError::Timeout { timeout_ms: 20 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_span_straddling_an_interior_window_boundary_increments_the_counter() {
        // 24 bytes, 16-byte windows, 4-byte overlap: two windows, one interior cut
        // at 16. The marker sits at 14..20 and therefore crosses it.
        let marker = "SECRET";
        let body = format!("{}{}{}", "a".repeat(14), marker, "a".repeat(4));
        let budgets = PrivacyBudgets::new(16, 4, 1_000, 2_000, 0, 20_000);

        let scan = scan(
            &detector(MarkerDetector { marker }),
            &body,
            Surface::Async,
            &budgets,
        )
        .await
        .expect("the marker detector does not fail");

        assert_eq!(scan.windows, 2, "the body must actually be cut in two");
        assert_eq!(
            scan.spans.len(),
            1,
            "one span comes back after fusion, not one per window that saw it"
        );
        assert_eq!(
            scan.stats.touching_window_edge, 1,
            "a span coming within one byte of an interior cut is counted"
        );
    }

    #[tokio::test]
    async fn a_single_window_body_reports_no_boundary_span() {
        let marker = "SECRET";
        let body = format!("{}{}{}", "a".repeat(4), marker, "a".repeat(4));
        let budgets = PrivacyBudgets::default();

        let scan = scan(
            &detector(MarkerDetector { marker }),
            &body,
            Surface::Async,
            &budgets,
        )
        .await
        .expect("the marker detector does not fail");

        assert_eq!(scan.windows, 1, "the default ceiling holds the whole body");
        assert_eq!(scan.spans.len(), 1);
        assert_eq!(
            scan.stats.touching_window_edge, 0,
            "no interior cut, nothing to count"
        );
    }

    #[tokio::test]
    async fn a_span_away_from_a_cut_is_not_counted() {
        // The marker ends two bytes before the cut at 16 — near it, not on it.
        let marker = "SECRET";
        let body = format!("{}{}{}", "a".repeat(8), marker, "a".repeat(4));
        let budgets = PrivacyBudgets::new(16, 4, 1_000, 2_000, 0, 20_000);

        let scan = scan(
            &detector(MarkerDetector { marker }),
            &body,
            Surface::Async,
            &budgets,
        )
        .await
        .expect("the marker detector does not fail");

        assert_eq!(scan.spans.len(), 1);
        assert_eq!(
            scan.stats.touching_window_edge, 0,
            "a span that does not reach the cut is not a boundary signal"
        );
    }

    #[tokio::test]
    async fn an_excluded_write_never_reaches_the_detector() {
        struct Panicking;

        impl PiiDetector for Panicking {
            fn detect(&self, _input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
                panic!("an excluded write must not be submitted to a detector");
            }
        }

        let budgets = PrivacyBudgets::default();
        let outcome = pass(
            &detector(Panicking),
            "code-demo",
            Section::Decisions,
            "body",
            &budgets,
        )
        .await
        .expect("an excluded write is not an error");
        assert!(outcome.is_none());
    }
}

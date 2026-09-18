//! The deferred privacy sweep — re-judging a note through the write primitive.
//!
//! ## Why a sweep exists at all
//!
//! A pass that did not complete marks its note [`privacy-unscanned`] instead of
//! guessing. Nothing about that mark is terminal: it says *this body was not judged*,
//! and the judgement is exactly what the deployment eventually owes it. The sweep is
//! the party that goes back and pays it — the product enqueues its own work, with no
//! human in the loop and no new edge out of this crate.
//!
//! ## The four clauses it is bound by
//!
//! 1. **Perimeter.** The sweep covers the server's vaults, and never a vault whose
//!    namespace starts with `code-`. That test belongs to the selection — the server
//!    holds the vault registry and the index, and this module holds neither — so it is
//!    applied there, once, before a job is enqueued.
//! 2. **Guard 0 again, after selection.** The selection's prefix test is a *filter*,
//!    not a substitute: [`Vault::rescan_privacy_in`] re-runs the real pass, which
//!    re-applies the guard on the note's own **resolved** namespace and section. A note
//!    that moved into a `code-*` vault, or into `identity`, between selection and
//!    re-judgement is therefore skipped by the same code that guards every write — and
//!    one that moved the other way is judged, not missed.
//! 3. **Idempotent re-judgement.** A note the sweep judges to the same thing it
//!    already carried is **not written at all**: no `updated`, no `ContentHash`, no
//!    index upsert, no `.history/` snapshot. This is the clause that lets the sweep
//!    run every few minutes over a corpus that is almost always already judged, and
//!    the reason [`rescan_privacy_in`] resolves the verdict on a clone before deciding
//!    to write. See [`privacy_block::same_block_modulo_scanned_at`] for the one field
//!    the comparison must exempt.
//! 4. **Bounded work.** One re-judgement handles at most the `limit` its job carries,
//!    and the tick that enqueues it is bounded in turn. Nothing here loops over a
//!    corpus: every bound is carried by the caller's job, and this module never
//!    enumerates.
//!
//! ## What it cannot do
//!
//! A sweep can only ever re-run the pass. Under D-11 that means it may add
//! [`personal-classified`](crate::privacy_block::CLASSIFIED_TAG) or retire
//! [`privacy-unscanned`](crate::privacy_block::UNSCANNED_TAG) — never declassify a
//! note, and never retire the payload marker except through the two paths a completed
//! pass already owns (nothing markable, or a human rejection that still binds). There
//! is no sweep-specific authority in this module, and no way to express one: the note
//! is written through [`write_note_judged`](crate::registry::Vault::write_note_judged),
//! the same funnel as every other write.

use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::note::Note;
use gradatum_core::pii::PiiSpan;
use gradatum_core::section::Section;

use crate::error::VaultError;
use crate::privacy_block::{self, Status};
use crate::privacy_observer::PrivacyPass;
use crate::registry::Vault;

/// One completed detection pass, as the write path carries it from the pass to the
/// verdict.
///
/// Both halves of the insertion point need the same value: the pass produces it before
/// the read-before-write, the verdict consumes it after. It is also what
/// [`write_note_judged`](crate::registry::Vault::write_note_judged) accepts from the
/// sweep, so that a re-judgement pays the detector once.
///
/// Crate-internal and not `Clone`-by-accident: the span list is the pass's own output,
/// moved rather than copied where it can be.
#[derive(Debug, Clone)]
pub(crate) struct PassOutcome {
    /// How the pass ended.
    pub(crate) status: Status,
    /// Validated, fused spans, in body order — empty for a failed pass.
    ///
    /// **After** the exception pass: a span a configured exception covers is not
    /// here. An exception is the operator's statement that a literal is not personal
    /// data, so a span it covers is not an observation the frontier holds — it is the
    /// false positive the exception was written to remove.
    pub(crate) spans: Vec<PiiSpan>,
    /// Labels the detector reported that no category matched, summed over the windows.
    pub(crate) unmapped_labels: u64,
    /// Spans rejected for falling outside the body.
    pub(crate) rejected_out_of_bounds: u64,
    /// Spans rejected for not sitting on a code-point boundary.
    pub(crate) rejected_utf8_boundary: u64,
    /// Spans sitting within one byte of an interior window boundary.
    pub(crate) touching_window_edge: u64,
    /// Spans the exception pass dropped, counted **per pattern identifier**.
    ///
    /// The identifier is a digest of the pattern, never the pattern: the exception
    /// lists are literal personal data by nature, and this map travels to a metric
    /// label and to a log line. The keys are sorted, so a report built from it does
    /// not vary with the body's span order.
    pub(crate) exception_hits: std::collections::BTreeMap<String, u64>,
}

/// What one re-judgement did with the note it was given.
///
/// The three states are the three answers the sweep's caller can act on, and they are
/// deliberately not four: a note that vanished between selection and re-judgement is
/// [`Skipped`](Self::Skipped) exactly like one the guard excludes, because there is
/// nothing to do about either and a caller that had to tell them apart could only
/// log it.
///
/// `#[non_exhaustive]`: a further state may be added without breaking a matcher
/// compiled against this one, and no crate outside this one can fabricate a verdict
/// the sweep never reached.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyRescanOutcome {
    /// The note was re-judged and its block or reserved tags changed: it was written.
    Rewritten,
    /// The note was re-judged to exactly what it already carried: nothing was written.
    Unchanged,
    /// No pass ran — the guard excluded the note, no detector is mounted, or the note
    /// is no longer there.
    Skipped,
}

impl PrivacyRescanOutcome {
    /// The label a caller reports this outcome under.
    ///
    /// A closed vocabulary, so a metric or a log line built from it carries three
    /// bounded values and never a note, a vault or a body.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rewritten => "rewritten",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
        }
    }
}

impl Vault {
    /// Runs one detection pass and reports what it saw — without writing, and without
    /// notifying the observer.
    ///
    /// The pass is the same [`crate::privacy::pass`] every write consults; the only
    /// thing this adds is the shape the two halves of the insertion point hand to each
    /// other. `None` means no pass ran at all — a `code-*` namespace, the `identity`
    /// section, or a detector that declares no identity — which is a different answer
    /// from a pass that ran and found nothing.
    ///
    /// A detector failure is **not** an error here, for the same reason it is not one
    /// in the write path: the note is owed a verdict either way, and the verdict of a
    /// failed pass is the rescan marker.
    pub(crate) async fn run_privacy_pass(
        &self,
        vault_id: &str,
        section: Section,
        body: &str,
    ) -> Option<PassOutcome> {
        match crate::privacy::pass(&self.detector, vault_id, section, body, &self.budgets).await {
            Ok(Some(scan)) => {
                // Last step of the pass, after fusion and after the thresholds have
                // had their say. An exception is applied to the spans themselves and
                // not to the marking decision alone: what it covers is declared not
                // personal, so it is no longer an observation — neither in the block,
                // nor in the counts, nor in the digest the next pass compares against.
                let (spans, exception_hits) =
                    self.privacy_policy.retain(vault_id, body, scan.spans);
                tracing::debug!(
                    section = section.as_str(),
                    windows = scan.windows,
                    spans = spans.len(),
                    merged = scan.stats.merged,
                    touching_window_edge = scan.stats.touching_window_edge,
                    exceptions = exception_hits.len(),
                    "F-160: detection pass complete"
                );
                Some(PassOutcome {
                    status: Status::Ok,
                    spans,
                    unmapped_labels: scan.unmapped_labels,
                    rejected_out_of_bounds: scan.stats.rejected_out_of_bounds as u64,
                    rejected_utf8_boundary: scan.stats.rejected_utf8_boundary as u64,
                    touching_window_edge: scan.stats.touching_window_edge as u64,
                    exception_hits,
                })
            }
            // Guard 0 (`code-*`, `identity` section) or a detector declaring no
            // identity: no pass — no block, no tag, and no removal of a block already
            // there. No exception is consulted either: there is no span to except.
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    section = section.as_str(),
                    error = %err,
                    "F-160: detection pass did not complete — the verdict is a rescan marker"
                );
                Some(PassOutcome {
                    status: Status::from_error(&err),
                    spans: Vec::new(),
                    unmapped_labels: 0,
                    rejected_out_of_bounds: 0,
                    rejected_utf8_boundary: 0,
                    touching_window_edge: 0,
                    exception_hits: std::collections::BTreeMap::new(),
                })
            }
        }
    }

    /// Rewrites the body of one write, when the policy destroys what it scanned.
    ///
    /// Returns the body to persist and whether anything was removed from it. Both are
    /// needed by the caller: the body because every byte written from here on — the
    /// [`ContentHash`](gradatum_core::identity::ContentHash), the file on disk, the note
    /// handed back — must describe the rewritten text rather than the text handed in; the
    /// flag because the version history a write normally keeps must not gain a copy of
    /// what this one destroyed.
    ///
    /// ## What it destroys, and what it leaves
    ///
    /// The spans the **activation marks**, and only those: the mode and the thresholds are
    /// one decision, so a category the operator did not activate, or calibrated at `0`,
    /// survives a masking write exactly as it survives a reporting one. A span a declared
    /// exception covered never reaches here at all — the pass applies exceptions to the
    /// spans before anything else reads them, so what an operator declared benign is not
    /// destroyed by the mode either.
    ///
    /// The block's ceiling applies here as it applies to the record: a pass that reported
    /// more spans than a block carries is `spans_truncated`, carries
    /// `privacy-unscanned`, and the sweep finishes the work. Masking past the ceiling
    /// would destroy text no note accounts for.
    ///
    /// ## No pass, no destruction
    ///
    /// `None` is a write that was excluded before any detector ran, or an installation
    /// with no detector mounted: there is no verdict to act on, and the body is returned
    /// untouched rather than emptied on a policy that was never consulted.
    pub(crate) fn masked_body(
        &self,
        body: String,
        outcome: Option<&PassOutcome>,
    ) -> (String, bool) {
        let Some(outcome) = outcome else {
            return (body, false);
        };
        if !self.privacy_policy.masks() {
            return (body, false);
        }
        let marked: Vec<PiiSpan> = outcome
            .spans
            .iter()
            .take(privacy_block::MAX_SPANS)
            .filter(|span| self.privacy_policy.marks(span))
            .copied()
            .collect();
        if marked.is_empty() {
            return (body, false);
        }
        let masked = gradatum_core::pii::mask_spans(&body, &marked);
        // Compared rather than assumed: the rewrite is the fact, and a caller that
        // suppressed a version snapshot on the strength of an empty marked list would be
        // hiding a history entry for a write that changed nothing.
        let changed = masked != body;
        (masked, changed)
    }

    // Insertion-point spec §5.1, steps 3/3ter/4 (F-160).
    /// Applies the insertion point's steps 3, 3ter and 4 to `frontmatter`.
    ///
    /// `existing` is the note as it was read back from the vault, or `None` for a
    /// creation. `outcome` is the pass, or `None` when none ran.
    ///
    /// This is the **only** implementation of those three steps. The write path and the
    /// sweep both call it — the sweep on a clone, to learn whether it has anything to
    /// write — so a re-judgement cannot reach a verdict an ordinary write would not.
    ///
    /// ## Step 3 — read-preservation of `extra`
    ///
    /// A rewrite carries over the keys it does not recompute: the source is the note as
    /// read back, never an echo of what the caller handed in. Without this re-carry, a
    /// path that rebuilds its frontmatter from scratch (the curate persist,
    /// `persist.rs:419`) would drop the keys the curation worker, the code map or a job
    /// had put there — silently, and with no pass to put them back. A key already present
    /// in the incoming frontmatter is never overwritten: what the caller knows about its
    /// own note wins over what it did not. The pass's block is written AFTER, in step 4.
    ///
    /// ## Step 3ter — re-carry of reserved tags (SEC-P0-2)
    ///
    /// A reserved tag belongs to no caller: it is posed and retired by a pass's verdict,
    /// never by the tag list a write site rebuilds. Seven paths nonetheless write by
    /// re-posting their tags as `String` — the curation worker (`apalis_handlers.rs:547`
    /// and `:771`), `vault_mutate_role` (`logic.rs:4934`), admin indexing
    /// (`reindex_orphans.rs:315`)… — and run them back through `Tag::normalize`: if one
    /// of them re-posted a list the marker had dropped out of, the rewrite would erase it
    /// with no pass having retired it. Since the absence of a detector is the ORDINARY
    /// state of an installation, no pass would ever put it back.
    ///
    /// One named exception: a `rejected` verdict that still covers the note's spans
    /// (path (b) of D-11, cf. [`privacy_block::binds_the_tag`]) binds the marker. This
    /// re-carry reads the verdict from the note as read back, NEVER from the incoming
    /// block: `spans_digest` is readable in the note, so a block that declares itself
    /// `rejected` proves nothing, and accepting it here would be enough to retire the
    /// marker in a single write with no human having judged. Only the review surface
    /// (`vault_privacy_review`, D-13) poses a verdict; until then, only what a verdict has
    /// already left on disk counts. Step 4 below is the one place the incoming block is
    /// read, and it is a different question: the pass asks what verdict the write states,
    /// not whether the marker must be restored.
    ///
    /// Accepted residual, bounded by the delivered surface: no write DTO carries an
    /// `extra` field and every production site builds `ExtraFields::empty()`, so no
    /// delivered surface can forge a `privacy.review` today.
    ///
    /// ## Step 4 — the block, and the two reserved tags
    ///
    /// The block states what the pass saw, never the value it saw. The verdict resolves
    /// both reserved tags in the same gesture: `status != ok` (a failed or truncated
    /// pass) poses `privacy-unscanned` and leaves `personal-classified` intact; an `ok`
    /// pass retires `privacy-unscanned` and is the authority for the payload marker only
    /// if it saw enough to pose it.
    ///
    /// ## Return
    ///
    /// The two resolved tag decisions — `(classified, unscanned)` — when a pass ran, and
    /// `None` when none did. Returned rather than recomputed by the caller because the
    /// observer reports what the verdict **decided**, and a second reading of the same
    /// inputs is a second authority on whether a marker was posed.
    #[must_use]
    pub(crate) fn decide_privacy(
        &self,
        frontmatter: &mut gradatum_core::frontmatter::Frontmatter,
        existing: Option<&Note>,
        outcome: Option<&PassOutcome>,
    ) -> Option<(privacy_block::TagAction, privacy_block::TagAction)> {
        if let Some(prev) = existing
            && let Some(prev_extra) = prev.frontmatter.extra.0.as_ref()
        {
            for (key, value) in prev_extra.iter() {
                if frontmatter.extra.get(key).is_none() {
                    frontmatter.extra.insert(key.clone(), value.clone());
                }
            }
        }

        // The block the note carried **before** this write, as read back from disk: the
        // only evidence that a verdict was ever posed, and the source of the re-carry
        // exception. Never the incoming block — a block that declares itself `rejected`
        // proves nothing, and honouring one here would be enough to retire the marker in
        // a single write with no human having judged.
        let previous_block = existing
            .as_ref()
            .and_then(|prev| prev.frontmatter.extra.get(privacy_block::BLOCK_KEY));
        let binds =
            previous_block.is_some_and(|block| privacy_block::binds_the_tag(block, previous_block));

        // What the pass re-judges. The note's own block on every ordinary path; the block
        // **entering** this write on the one surface that posts a block back — the review,
        // which moves `review` over the block the note carries and states it. Resolving
        // the verdict from disk on that path would recompute `pending` over it and undo a
        // human judgement on the very write that poses it.
        //
        // What may be stated is bounded, not assumed: an entering block counts only when
        // it moves the verdict over a block the note already carries, so a block the
        // caller composed reads as no verdict and the pass re-judges from disk. That
        // bound is what keeps the exception narrow — the pass is not an override any
        // write can aim.
        //
        // `spans_digest` is the remaining guard, and it is what makes this a judgement
        // rather than a blanket carry: a verdict is kept only while it covers the very
        // spans the pass re-reported, so a note whose spans moved spends it.
        let carried_block = match frontmatter.extra.get(privacy_block::BLOCK_KEY) {
            Some(incoming)
                if previous_block.is_some_and(|recorded| {
                    privacy_block::moves_only_the_review(incoming, recorded)
                }) =>
            {
                Some(incoming)
            }
            _ => previous_block,
        };

        if let Some(prev) = existing {
            // La valeur réinsérée est celle que porte la note relue — clonée telle
            // quelle, jamais reconstruite depuis sa forme textuelle. Un tag réservé ne
            // repasse donc par aucune normalisation, et le garde de réservation de
            // `Tag::normalize` (D-12), qui refusera ce littéral sur le chemin des
            // appelants, ne peut pas tarir ce re-report.
            for tag in &prev.frontmatter.tags {
                if !privacy_block::is_reserved(tag) {
                    continue;
                }
                if binds && tag.as_str() == privacy_block::CLASSIFIED_TAG.as_ref() {
                    continue;
                }
                if !frontmatter.tags.contains(tag) {
                    frontmatter.tags.push(tag.clone());
                }
            }
        }

        if let Some(outcome) = outcome {
            // Une passe a eu lieu, donc une identité a été déclarée : `privacy::pass`
            // rend `Ok(None)` sinon. La relecture est la même source unique, appelée une
            // seconde fois plutôt que recopiée de la passe. Une identité absente ici est
            // inatteignable ; elle laisse la note intacte plutôt que de deviner un
            // verdict, et ne rend aucune décision à observer.
            if let Some(identity) = self.detector.identity() {
                let decided = privacy_block::verdict(
                    &identity,
                    outcome.status,
                    &outcome.spans,
                    &self.budgets,
                    &self.privacy_policy,
                    carried_block,
                );
                // Une réécriture ne duplique pas `privacy-unscanned` : chaque action est
                // idempotente sur la liste de tags.
                privacy_block::apply_reserved_tag(
                    &mut frontmatter.tags,
                    privacy_block::UNSCANNED_TAG,
                    decided.unscanned,
                );
                privacy_block::apply_reserved_tag(
                    &mut frontmatter.tags,
                    privacy_block::CLASSIFIED_TAG,
                    decided.classified,
                );
                frontmatter
                    .extra
                    .insert(privacy_block::BLOCK_KEY.to_owned(), decided.block);
                return Some((decided.classified, decided.unscanned));
            }
        }

        if binds {
            // Aucune passe : le bloc et les tags transitent tels quels, à la seule
            // exception du verdict liant — un rejet non dépensé retire le marqueur, même
            // si l'appelant vient de le reposer.
            privacy_block::apply_reserved_tag(
                &mut frontmatter.tags,
                privacy_block::CLASSIFIED_TAG,
                privacy_block::TagAction::Retire,
            );
        }
        None
    }

    /// Reports one pass to the installed observer, if any.
    ///
    /// `block` is the block the verdict just resolved, and the status is read from it
    /// rather than from the pass: the span ceiling is applied while the block is built,
    /// so a pass that reported more spans than a block carries is `spans_truncated`
    /// here — the state a reader of the note will actually find.
    ///
    /// `classified` and `unscanned` are the decisions [`decide_privacy`](Self::decide_privacy)
    /// returned, so the report states what the note ended up carrying rather than what the
    /// pass alone would have suggested.
    ///
    /// Called at most once per pass, never from the sweep's dry re-judgement.
    pub(crate) fn observe_pass(
        &self,
        outcome: &PassOutcome,
        block: &toml::Value,
        classified: privacy_block::TagAction,
        unscanned: privacy_block::TagAction,
    ) {
        let Some(observer) = self.privacy_observer.as_ref() else {
            return;
        };
        let status = block
            .get("status")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        observer.pass_observed(
            &PrivacyPass::new(
                status,
                privacy_block::markable_categories(&outcome.spans, &self.privacy_policy),
                classified == privacy_block::TagAction::Pose,
                unscanned == privacy_block::TagAction::Pose,
                outcome.unmapped_labels,
                outcome.rejected_out_of_bounds,
                outcome.rejected_utf8_boundary,
                outcome.touching_window_edge,
            )
            .with_exception_hits(outcome.exception_hits.clone()),
        );
    }

    /// Re-judges one note of `vault_id` through the write primitive.
    ///
    /// Reads the note, runs the pass on its body, resolves the verdict on a **clone**,
    /// and writes only if the resolution differs from what the note already carries.
    ///
    /// ## Errors
    ///
    /// - `VaultError` from the read or from the write. A note that is no longer there
    ///   is not an error: it is [`PrivacyRescanOutcome::Skipped`], because the sweep has
    ///   nothing to do about a note deleted between its selection and its turn, and a
    ///   failing job would misreport that as a broken sweep.
    ///
    /// ## What it will not do
    ///
    /// It never writes an unchanged note, and it never reaches the vault by any path
    /// other than `write_note_judged` — the same funnel, the
    /// same guards, the same tags. See the module documentation for the four clauses.
    pub async fn rescan_privacy_in(
        &self,
        vault_id: &str,
        id: NoteId,
    ) -> Result<PrivacyRescanOutcome, VaultError> {
        let note = match self.read_note_in(vault_id, id).await {
            Ok(note) => note,
            Err(VaultError::Core(GradatumError::NoteNotFound(_))) => {
                return Ok(PrivacyRescanOutcome::Skipped);
            }
            Err(other) => return Err(other),
        };

        // Clause 2 — the guard is re-applied on the note's own resolved namespace, by the
        // pass itself. The selection that offered this note filtered on the same prefix,
        // and that is exactly why it is not trusted here: a note can move between the
        // two moments, and the moment that matters is this one.
        let write_vault = if note.frontmatter.vault_id.as_str().is_empty() {
            self.vault_id.clone()
        } else {
            note.frontmatter.vault_id.clone()
        };
        let body = note.body.markdown.clone();
        let section = note.frontmatter.section;

        let Some(outcome) = self
            .run_privacy_pass(write_vault.as_str(), section, &body)
            .await
        else {
            return Ok(PrivacyRescanOutcome::Skipped);
        };

        // Clause 3 — resolved on a clone. `decide_privacy` is the same function the write
        // path calls, so the preview and the write agree by construction; the comparison
        // exempts `scanned_at`, the one field a pass re-stamps on every run.
        //
        // Équivalence bloc ⇒ tags, qui est ce qui autorise à ne comparer QUE le bloc :
        // `decide_privacy` ne fusionne pas ce bloc, il l'ÉCRASE depuis le même `decided`
        // dont les deux tags réservés sont tirés (`apply_reserved_tag` juste au-dessus de
        // l'insertion). Bloc et tags sont donc deux projections d'une seule valeur, et
        // `verdict()` la calcule sur des entrées — statut de passe, spans scorés, verdict
        // humain liant — que le bloc porte toutes : un statut `ok`/échoué y est écrit, les
        // spans y sont (c'est ce que `markable` lit), un rejet liant y est consigné. Deux
        // blocs égaux modulo `scanned_at` ont donc les mêmes entrées de verdict, et par
        // suite les mêmes actions de tag. La prévisualisation et l'écriture, elles, sont
        // le même appel sur le même `frontmatter`, la même note relue et le même
        // `outcome` : il n'y a pas deux calculs à faire coïncider, il n'y en a qu'un.
        //
        // Résiduel, borné : un bloc forgé à la main qui serait déjà octet pour octet
        // celui qu'une passe produirait laisserait les tags qu'il contredit en place
        // jusqu'à la prochaine écriture — laquelle repasse par `decide_privacy` et les
        // corrige. La comparaison ne peut donc pas figer une incohérence qu'elle aurait
        // créée ; elle ne peut que différer une correction qu'aucun chemin n'a faite.
        let mut preview = note.frontmatter.clone();
        let _ = self.decide_privacy(&mut preview, Some(&note), Some(&outcome));

        let before = note.frontmatter.extra.get(privacy_block::BLOCK_KEY);
        let after = preview.extra.get(privacy_block::BLOCK_KEY);
        let identical = match (before, after) {
            (Some(before), Some(after)) => {
                privacy_block::same_block_modulo_scanned_at(before, after)
            }
            (None, None) => true,
            _ => false,
        };
        if identical {
            return Ok(PrivacyRescanOutcome::Unchanged);
        }

        self.write_note_judged(note.frontmatter, body, id, Some(outcome))
            .await?;
        Ok(PrivacyRescanOutcome::Rewritten)
    }
}

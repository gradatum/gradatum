//! Detector implementations and span geometry for the gradatum personal-data
//! detection frontier.
//!
//! [`RegexDetector`] implements [`gradatum_core::pii::PiiDetector`] with a small,
//! deterministic set of patterns (e-mail, IBAN, French phone numbers, French
//! social-security numbers). It is always available and depends on no external
//! process, so it can serve as a floor beneath a heavier model detector.
//!
//! Every regex-produced span carries the maximal score (`1.0`): the patterns are
//! exact by construction and are not subject to a probability threshold.
//!
//! The same crate carries the geometry a detector needs around it. [`WindowPlan`]
//! cuts a body larger than a detector's per-call ceiling into overlapping windows;
//! [`rebase`] moves the offsets a detector returned from window coordinates onto
//! the body; [`validate_and_merge`] rejects what cannot be applied and fuses
//! overlapping spans of the same category, across windows as well as within one.

mod merge;
mod regex_detector;
mod windowing;

pub use merge::{MergeStats, rebase, validate_and_merge};
pub use regex_detector::RegexDetector;
pub use windowing::WindowPlan;

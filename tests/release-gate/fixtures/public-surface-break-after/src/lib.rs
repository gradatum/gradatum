//! Fixture APRÈS — la variante `#[non_exhaustive]` change de champ (OldPayload → NewPayload).
//!
//! C'est la rupture de compilation certaine que `cargo-semver-checks` ne voit pas
//! (196 checks pass, « no semver update required ») alors que `cargo public-api` la
//! rend dans le diff de surface.

pub struct OldPayload(pub u32);
pub struct NewPayload(pub String);

#[non_exhaustive]
pub enum BreakError {
    Variant(NewPayload),
}

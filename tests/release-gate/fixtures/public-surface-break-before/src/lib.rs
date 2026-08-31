//! Fixture AVANT — reproduit la classe de rupture F-145.
//!
//! Les DEUX types (`OldPayload`, `NewPayload`) existent dans les deux versions — le type
//! porte-rupture reste dans le graphe, comme `sqlx::Error` restait dans le graphe après
//! les sous-lots F-145 1/2/3. Seule la variante `#[non_exhaustive]` change de champ.

pub struct OldPayload(pub u32);
pub struct NewPayload(pub String);

#[non_exhaustive]
pub enum BreakError {
    Variant(OldPayload),
}

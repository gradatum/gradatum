//! Estimation de tokens pluggable. Défaut v0.7.0 : heuristique calibrée
//! (words × 1.3, bounded by character count). An `ExactTokenizer` hook is reserved but not yet implemented (YAGNI).
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> u32;
}

pub struct HeuristicEstimator;

impl TokenEstimator for HeuristicEstimator {
    fn estimate(&self, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        let words = text.split_whitespace().count() as f64;
        let chars = text.chars().count() as f64;
        // Plancher chars/6 : protège contre les textes denses sans espaces (ex. code,
        // blocs répétitifs) où `split_whitespace` retourne très peu de "mots" malgré
        // un corps volumineux. Un mot moyen FR/EN = ~5-6 chars → chars/6 ≈ nb_mots réels.
        // Plafond chars/2 : sous-word units, ponctuation.
        ((words * 1.3).max(chars / 6.0).min(chars / 2.0).max(1.0)).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_estimate_is_nonzero_and_below_chars() {
        let est = HeuristicEstimator;
        let text = "The quick brown fox jumps over the lazy dog.";
        let n = est.estimate(text);
        assert!(n >= 1);
        // calibration : ~ mots*1.3, doit rester < nb de chars
        assert!(n < text.chars().count() as u32);
    }

    #[test]
    fn heuristic_empty_is_zero() {
        assert_eq!(HeuristicEstimator.estimate(""), 0);
    }

    // Texte dense sans espaces (ex. séquence répétitive) : le plancher chars/6
    // garantit une estimation raisonnable même si split_whitespace retourne 1 mot.
    #[test]
    fn heuristic_dense_text_uses_char_floor() {
        let dense = "x".repeat(600); // 600 chars, 1 "mot"
        let n = HeuristicEstimator.estimate(&dense);
        // chars/6 = 100 → plancher activé, pas 1 (serait le résultat de words*1.3 = 1.3)
        assert!(
            n >= 50,
            "texte dense 600 chars doit estimer > 50 tokens, got {n}"
        );
        // plafond chars/2 = 300 respecté
        assert!(n <= 300, "estimation doit être ≤ chars/2 = 300, got {n}");
    }
}

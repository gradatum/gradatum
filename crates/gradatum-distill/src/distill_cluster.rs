//! Cosine clustering for semantic distillation.
//!
//! Groups notes by cosine similarity of their embeddings, using connected components
//! of an adjacency graph (pair linked iff cosine ≥ threshold).
//!
//! ## Complexity
//!
//! `O(n²)` pairs compared, bounded upstream by `batch_limit` (≤ 500 in practice).
//! Union-find makes component aggregation nearly linear after the pair computation.
//!
//! ## Purity
//!
//! No I/O or worker dependency — fully unit-testable.

/// Computes the cosine similarity between two vectors.
///
/// Returns `0.0` if either vector has zero norm or if the dimensions differ
/// (degenerate case — no signal, no grouping).
///
/// # Return value
///
/// `f32` in `[-1.0, 1.0]` for valid vectors of the same dimension.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Union-Find (disjoint-set) with path compression — used to aggregate clusters.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        // Path compression.
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Groups indices `[0, embeddings.len())` into clusters where cosine ≥ `threshold`.
///
/// Two notes are linked iff their cosine is `>= threshold`. Clusters are the
/// connected components of the resulting graph. An isolated note forms a singleton cluster.
///
/// # Parameters
///
/// - `embeddings`: one vector per candidate note (same dimension expected).
/// - `threshold`: cosine grouping threshold (typically `0.75`).
///
/// # Return value
///
/// A list of clusters, each being a list of indices (sorted ascending)
/// into `embeddings`. Cluster order is deterministic (by smallest index, ascending).
/// Singletons are included.
///
/// # Bounds
///
/// `O(n²)` pair comparisons — the caller MUST bound `embeddings.len()` via
/// `batch_limit`.
#[must_use]
pub fn cluster_by_cosine(embeddings: &[Vec<f32>], threshold: f32) -> Vec<Vec<usize>> {
    let n = embeddings.len();
    if n == 0 {
        return Vec::new();
    }
    let mut uf = UnionFind::new(n);
    for i in 0..n {
        for j in (i + 1)..n {
            if cosine_similarity(&embeddings[i], &embeddings[j]) >= threshold {
                uf.union(i, j);
            }
        }
    }

    // Group by root, preserving deterministic order (by minimum index).
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }

    // Sort clusters by their minimum index for a stable order, independent
    // of the union-find root chosen.
    let mut clusters: Vec<Vec<usize>> = groups.into_values().collect();
    for c in &mut clusters {
        c.sort_unstable();
    }
    clusters.sort_by_key(|c| c[0]);
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_norm_returns_zero() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_mismatched_dims_returns_zero() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cluster_empty_input_returns_empty() {
        let clusters = cluster_by_cosine(&[], 0.75);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cluster_singleton_when_no_similarity() {
        // Trois vecteurs orthogonaux → trois singletons.
        let embeddings = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let clusters = cluster_by_cosine(&embeddings, 0.75);
        assert_eq!(
            clusters.len(),
            3,
            "trois singletons attendus : {clusters:?}"
        );
        for c in &clusters {
            assert_eq!(c.len(), 1);
        }
    }

    #[test]
    fn cluster_groups_similar_vectors() {
        // 0 et 1 quasi-identiques (cosine ≈ 1) ; 2 orthogonal.
        let embeddings = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.99, 0.01, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let clusters = cluster_by_cosine(&embeddings, 0.75);
        assert_eq!(clusters.len(), 2, "un duo + un singleton : {clusters:?}");
        // Le cluster contenant 0 doit aussi contenir 1.
        let duo = clusters
            .iter()
            .find(|c| c.contains(&0))
            .expect("cluster de 0");
        assert!(duo.contains(&1), "0 et 1 doivent être regroupés : {duo:?}");
    }

    #[test]
    fn cluster_transitive_chaining() {
        // Chaîne 0~1, 1~2 (mais 0 et 2 sous le seuil direct) → un seul cluster transitif.
        let embeddings = vec![
            vec![1.0, 0.0],
            vec![0.8, 0.6], // cosine(0,1)=0.8 ; cosine(1,2)=0.8 ; cosine(0,2)≈0.28
            vec![0.28, 0.96],
        ];
        // Seuil 0.75 : (0,1) et (1,2) reliés, (0,2) non — union transitive.
        let clusters = cluster_by_cosine(&embeddings, 0.75);
        assert_eq!(
            clusters.len(),
            1,
            "chaînage transitif → un seul cluster : {clusters:?}"
        );
        assert_eq!(clusters[0], vec![0, 1, 2]);
    }

    #[test]
    fn cluster_order_is_deterministic() {
        let embeddings = vec![vec![0.0, 1.0], vec![1.0, 0.0]];
        let clusters = cluster_by_cosine(&embeddings, 0.99);
        // Deux singletons, triés par indice min : [0], [1].
        assert_eq!(clusters, vec![vec![0], vec![1]]);
    }
}

//! Ranking of search-found evidence: reciprocal rank fusion of the lexical and vector searches,
//! then one step of relevance spreading along graph links.
//!
//! When a question names nothing Anchor can resolve, its starting points come from searches
//! (node names, full text, and vectors when a neural embedding model is configured). Each
//! search ranks well on its own terms, so the lists are merged by reciprocal rank fusion, which
//! needs no score calibration between them. A passage linked from a strong candidate is often
//! the second half of a multi-hop answer while sharing few words with the question, so each top
//! candidate passes part of its score to the nodes it is linked to, in proportion to the link's
//! weight: a one-step, seed-personalised PageRank.

use std::collections::HashMap;

use crate::base::NodeId;

/// Rank offset of reciprocal rank fusion. Small values let the top of each list dominate.
pub const RRF_K: f32 = 10.0;
/// Fraction of a candidate's score passed along a link of weight 1.
pub const SPREAD: f32 = 0.75;
/// How many of the best candidates pass on relevance.
pub const SPREAD_FROM: usize = 5;

/// Reciprocal rank fusion of best-first lists: `Σ 1 / (k + rank + 1)` per node, scaled so the
/// best node scores 1. Ties keep the order in which nodes first appear.
pub fn rrf(rankings: &[Vec<NodeId>], k: f32) -> Vec<(NodeId, f32)> {
    let mut scores: HashMap<NodeId, (f32, usize)> = HashMap::new();
    let mut order = 0;
    for ranking in rankings {
        for (rank, node) in ranking.iter().enumerate() {
            let entry = scores.entry(*node).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            entry.0 += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    sorted(scores)
}

/// Adds to every node the relevance its links bring from the best `from` candidates
/// (`score × weight × damping`, summed over the candidates linking to it). Returns every
/// candidate and every node reached, best first, scaled so the best scores 1.
pub fn spread<I>(
    candidates: &[(NodeId, f32)],
    links: impl Fn(NodeId) -> I,
    from: usize,
    damping: f32,
) -> Vec<(NodeId, f32)>
where
    I: IntoIterator<Item = (NodeId, f32)>,
{
    let mut scores: HashMap<NodeId, (f32, usize)> = HashMap::new();
    for (order, (node, score)) in candidates.iter().enumerate() {
        scores.insert(*node, (*score, order));
    }
    let mut order = candidates.len();
    for (node, score) in candidates.iter().take(from) {
        for (linked, weight) in links(*node) {
            if linked == *node {
                continue;
            }
            let entry = scores.entry(linked).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            entry.0 += score * weight.clamp(0.0, 1.0) * damping;
        }
    }
    sorted(scores)
}

fn sorted(scores: HashMap<NodeId, (f32, usize)>) -> Vec<(NodeId, f32)> {
    let mut out: Vec<(NodeId, f32, usize)> = scores
        .into_iter()
        .map(|(node, (score, order))| (node, score, order))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    let best = out.first().map_or(1.0, |first| first.1).max(f32::EPSILON);
    out.into_iter()
        .map(|(node, score, _)| (node, score / best))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: i64) -> NodeId {
        NodeId(id)
    }

    #[test]
    fn a_node_ranked_well_by_both_searches_wins() {
        let fused = rrf(&[vec![n(1), n(2), n(3)], vec![n(2), n(4)]], RRF_K);
        let order: Vec<NodeId> = fused.iter().map(|(node, _)| *node).collect();
        assert_eq!(order, vec![n(2), n(1), n(4), n(3)]);
        assert!((fused[0].1 - 1.0).abs() < 1e-6);
        assert!(rrf(&[], RRF_K).is_empty());
    }

    #[test]
    fn a_node_linked_from_the_best_candidates_rises() {
        // 1 and 2 are found by search; 9 is found by neither but both link to it.
        let candidates = vec![(n(1), 1.0), (n(2), 0.9), (n(3), 0.5)];
        let links = |node: NodeId| match node.0 {
            1 | 2 => vec![(n(9), 1.0)],
            _ => vec![],
        };
        let ranked = spread(&candidates, links, 5, 0.5);
        let order: Vec<NodeId> = ranked.iter().map(|(node, _)| *node).collect();
        assert_eq!(order, vec![n(1), n(9), n(2), n(3)]);

        // Without links, the search order stands.
        let alone = spread(&candidates, |_| Vec::new(), 5, 0.5);
        assert_eq!(
            alone.iter().map(|(node, _)| *node).collect::<Vec<_>>(),
            vec![n(1), n(2), n(3)]
        );
    }

    #[test]
    fn only_the_best_candidates_spread_and_weak_links_carry_less() {
        let candidates = vec![(n(1), 1.0), (n(2), 0.2)];
        let links = |node: NodeId| match node.0 {
            1 => vec![(n(8), 0.2)],
            2 => vec![(n(9), 1.0)],
            _ => vec![],
        };
        let ranked = spread(&candidates, links, 1, 0.5);
        assert!(ranked.iter().all(|(node, _)| *node != n(9)), "{ranked:?}");
        let eight = ranked.iter().find(|(node, _)| *node == n(8)).unwrap().1;
        assert!((eight - 0.1).abs() < 1e-6, "{eight}");
    }
}

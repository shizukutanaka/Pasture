//! Embedding-based difficulty signal (IMP-14, opt-in, off by default).
//!
//! The deterministic routing heuristics catch prompts that *look* hard (code
//! fences, math, multi-question, reasoning keywords). They miss hard prompts
//! that read like plain prose. This module adds an optional second signal:
//! the user lists prompts their local model handled badly (one per line in
//! `PASTURE_HARD_PROMPTS`); a request whose embedding — from the local
//! backend's `/v1/embeddings`, the same IMP-12 infrastructure as the semantic
//! cache — is within `PASTURE_HARD_THRESHOLD` cosine similarity of any of
//! those "known-hard" centroids escalates to the cloud before wasting a local
//! attempt. The deterministic path remains the always-on baseline; this signal
//! only ever escalates Local → Cloud, never the reverse, and never overrides
//! privacy (sensitive prompts stay local regardless).
//!
//! Grounded in the routing survey's clustering paradigm (arXiv:2603.04445)
//! and embedding routers (vLLM Semantic Router); Pasture's variant needs no
//! training — just examples — and stays zero-dependency (local embeddings).

use crate::cache::cosine_similarity;

/// Load known-hard prompts from a plain-text file: one prompt per line.
/// Blank lines and lines starting with `#` are skipped.
pub fn load_hard_prompts(path: &str) -> Result<Vec<String>, String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    Ok(body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// The highest cosine similarity between `query` and any centroid, when it
/// meets `threshold`. `None` when no centroid is close enough (or there are
/// no centroids at all).
pub fn similar_to_hard(query: &[f64], centroids: &[Vec<f64>], threshold: f64) -> Option<f64> {
    let best = centroids
        .iter()
        .map(|c| cosine_similarity(query, c))
        .fold(f64::NEG_INFINITY, f64::max);
    (best.is_finite() && best >= threshold).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_similar_to_hard_hit_returns_best() {
        let centroids = vec![vec![0.0, 1.0], vec![1.0, 0.0]];
        // Identical to the second centroid → cosine 1.0.
        let sim = similar_to_hard(&[1.0, 0.0], &centroids, 0.9);
        assert!((sim.unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_similar_to_hard_below_threshold() {
        // 45° from both axes → cosine ~0.707 against either centroid.
        let centroids = vec![vec![0.0, 1.0], vec![1.0, 0.0]];
        assert!(similar_to_hard(&[1.0, 1.0], &centroids, 0.9).is_none());
        // The same query passes a looser threshold.
        assert!(similar_to_hard(&[1.0, 1.0], &centroids, 0.7).is_some());
    }

    #[test]
    fn test_similar_to_hard_no_centroids() {
        assert!(similar_to_hard(&[1.0, 0.0], &[], 0.0).is_none());
    }

    fn tmp_prompts(name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        let p = std::env::temp_dir().join(format!("pasture_hard_{name}.txt"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn test_load_hard_prompts_skips_blank_and_comments() {
        let p = tmp_prompts(
            "basic",
            "# prompts my 3B model fumbles\n\nsummarise the themes of Hamlet\n  walk me through tax brackets  \n",
        );
        let prompts = load_hard_prompts(p.to_str().unwrap()).unwrap();
        assert_eq!(
            prompts,
            vec![
                "summarise the themes of Hamlet".to_string(),
                "walk me through tax brackets".to_string(),
            ]
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_load_hard_prompts_missing_file() {
        assert!(load_hard_prompts("/no/such/hard.txt").is_err());
    }

    #[test]
    fn test_load_hard_prompts_empty_file() {
        let p = tmp_prompts("empty", "# only a comment\n");
        assert_eq!(load_hard_prompts(p.to_str().unwrap()).unwrap().len(), 0);
        let _ = std::fs::remove_file(&p);
    }
}

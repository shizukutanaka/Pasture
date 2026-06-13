//! Cascade confidence judging (IMP-1, grounded in FrugalGPT arXiv:2305.05176
//! and Confident-or-Seek-Stronger arXiv:2502.04428).
//!
//! v0 uses response heuristics — no logprobs required. The local model answers
//! first; if the answer looks low-confidence, the request escalates to the
//! cloud. The orchestration lives in the proxy/CLI (where the backends are);
//! this module provides the pure, testable judgement.

/// Uncertainty / refusal markers (EN + JA) that suggest the local model could
/// not answer well and the request should escalate.
const UNCERTAINTY_MARKERS: &[&str] = &[
    "i don't know",
    "i do not know",
    "i'm not sure",
    "i am not sure",
    "cannot answer",
    "can't answer",
    "no information",
    "not enough information",
    "as an ai",
    "i'm unable",
    "i am unable",
    "i cannot provide",
    "申し訳",
    "わかりません",
    "分かりません",
    "不明",
    "情報があり",
    "お答えでき",
];

/// True when the local answer looks low-confidence and should escalate.
/// Conservative: only empty/near-empty answers or explicit uncertainty markers
/// trigger escalation, to avoid wasting cloud calls on good short answers.
pub fn is_low_confidence(answer: &str) -> bool {
    let trimmed = answer.trim();
    if trimmed.chars().count() < 2 {
        return true;
    }
    let lower = trimmed.to_lowercase();
    UNCERTAINTY_MARKERS.iter().any(|m| lower.contains(m))
}

/// Decide whether to escalate a local answer to the cloud. Prefers the
/// research-backed mean token log-probability signal when the backend provides
/// it (arXiv 2605.02241: average log-prob matches or beats supervised routers
/// for local->cloud routing, with no training data). When unavailable (e.g.
/// Ollama without logprobs), falls back to the text heuristic.
///
/// `mean_logprob` is <= 0; escalate when it drops below `logprob_threshold`.
///
/// An empty / near-empty answer always escalates, *regardless* of the logprob:
/// the two signals measure different things — a model can emit nothing (or just
/// an EOS token) while reporting a high token-confidence, and an empty answer is
/// never useful to return. Without this guard a confident empty response
/// (`mean_logprob` above threshold) would stay local and hand the user a blank
/// answer when the cloud could have answered (ADR-146). The logprob signal only
/// governs the *non-empty* case, where it adds escalation for subtle uncertainty
/// the text heuristic cannot see.
pub fn should_escalate(answer: &str, mean_logprob: Option<f64>, logprob_threshold: f64) -> bool {
    if answer.trim().chars().count() < 2 {
        return true;
    }
    match mean_logprob {
        Some(lp) => lp < logprob_threshold,
        None => is_low_confidence(answer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_escalate_uses_logprob_when_present() {
        // Confident (high logprob) stays local; uncertain (low) escalates.
        assert!(!should_escalate("anything", Some(-0.2), -1.0));
        assert!(should_escalate("anything", Some(-1.5), -1.0));
        // Exactly at threshold does not escalate (strict <).
        assert!(!should_escalate("anything", Some(-1.0), -1.0));
    }

    #[test]
    fn test_empty_answer_escalates_despite_confident_logprob() {
        // ADR-146: a model can emit nothing while reporting high token confidence.
        // An empty / near-empty answer must escalate regardless of the logprob —
        // the logprob must never suppress the empty-answer signal.
        assert!(should_escalate("", Some(0.0), -1.0));
        assert!(should_escalate("   ", Some(-0.1), -1.0));
        assert!(should_escalate("x", Some(0.0), -1.0));
        // Control: a confident *non-empty* answer with a good logprob stays local.
        assert!(!should_escalate("Paris.", Some(-0.1), -1.0));
    }

    #[test]
    fn test_should_escalate_falls_back_to_heuristic() {
        // No logprob -> use text heuristic.
        assert!(should_escalate("I don't know", None, -1.0));
        assert!(!should_escalate(
            "Paris is the capital of France.",
            None,
            -1.0
        ));
    }

    #[test]
    fn test_empty_is_low_confidence() {
        assert!(is_low_confidence(""));
        assert!(is_low_confidence("  "));
        assert!(is_low_confidence("x"));
    }

    #[test]
    fn test_uncertainty_markers_en() {
        assert!(is_low_confidence("I don't know the answer to that."));
        assert!(is_low_confidence(
            "Sorry, I cannot provide that information."
        ));
        assert!(is_low_confidence("As an AI, I am unable to help."));
    }

    #[test]
    fn test_uncertainty_markers_ja() {
        assert!(is_low_confidence("申し訳ありませんが、わかりません。"));
        assert!(is_low_confidence("その情報がありません。"));
    }

    #[test]
    fn test_confident_answer_not_low() {
        assert!(!is_low_confidence("The capital of France is Paris."));
        assert!(!is_low_confidence("Paris."));
        assert!(!is_low_confidence("42"));
    }

    #[test]
    fn test_case_insensitive() {
        assert!(is_low_confidence("I DON'T KNOW"));
    }
}

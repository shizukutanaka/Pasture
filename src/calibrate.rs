//! Data-driven threshold calibration.
//!
//! The routing engine sends a prompt to the cloud when its estimated token
//! count reaches a threshold. The default threshold is hardware-derived, but
//! the *right* threshold depends on the user's own prompt mix and their cost
//! tolerance. This module derives a length threshold from the distribution of
//! prompt sizes the user has actually seen (recorded as token counts in the
//! PII-free cost log) so that a target fraction of future, similar prompts
//! would be routed to the cloud.
//!
//! This is a deliberately simple, offline, zero-dependency analogue of the
//! cost/quality calibration studied in the routing literature (RouterBench,
//! UCCI). Learned routers (RouteLLM, Hybrid LLM) do better, but require
//! preference or quality-gap labels that a single local user does not have.
//! What we *can* do without labels is calibrate the one knob we have — the
//! length threshold — against real usage. The estimate is length-only: the
//! engine also escalates on content signals (reasoning/code/privacy), so the
//! realised cloud rate will be at least this much.

/// Recommend a token threshold so that about `target_cloud_rate` of the given
/// prompts (by length alone) would route to the cloud.
///
/// Returns `(threshold, achieved_rate)` where `achieved_rate` is the actual
/// fraction of `tokens` greater than or equal to the chosen threshold (it may
/// differ from the target when sizes are tied or the sample is small).
///
/// `target_cloud_rate` is clamped to `[0.0, 1.0]`. With an empty sample the
/// result is `(0, 0.0)`.
pub fn calibrate_threshold(tokens: &[u64], target_cloud_rate: f64) -> (usize, f64) {
    if tokens.is_empty() {
        return (0, 0.0);
    }
    let target = target_cloud_rate.clamp(0.0, 1.0);
    let n = tokens.len();

    // Send everything to the cloud.
    if target >= 1.0 {
        return (0, 1.0);
    }
    // Send nothing to the cloud: threshold just above the largest prompt.
    if target <= 0.0 {
        let max = *tokens.iter().max().unwrap();
        let threshold = max.saturating_add(1) as usize;
        return (threshold, 0.0);
    }

    let mut sorted: Vec<u64> = tokens.to_vec();
    sorted.sort_unstable();

    // We want the smallest threshold T such that the fraction of prompts with
    // tokens >= T is <= target. Walk the (1 - target) quantile.
    let idx = (((1.0 - target) * n as f64).floor() as usize).min(n - 1);
    let threshold = sorted[idx];
    let cloud = sorted.iter().filter(|&&t| t >= threshold).count();
    let achieved = cloud as f64 / n as f64;
    (threshold as usize, achieved)
}

/// Recommend a cascade log-probability threshold so that about
/// `target_escalation_rate` of answers (by the local model's mean log-prob)
/// would escalate to the cloud. The cascade escalates when mean logprob is
/// *below* the threshold, so this is the lower-tail quantile of `logprobs`.
///
/// Returns `(threshold, achieved_rate)`. `target` is clamped to `[0,1]`; an
/// empty sample yields `(0.0, 0.0)`. This calibrates to an escalation *budget*
/// from the user's own distribution — a label-free analogue of the calibrated
/// cost-optimal routing in the literature (which needs correctness labels).
pub fn calibrate_logprob_threshold(logprobs: &[f64], target_escalation_rate: f64) -> (f64, f64) {
    if logprobs.is_empty() {
        return (0.0, 0.0);
    }
    let target = target_escalation_rate.clamp(0.0, 1.0);
    let n = logprobs.len();
    let mut sorted: Vec<f64> = logprobs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if target <= 0.0 {
        // Escalate nothing: threshold just below the smallest observed logprob.
        return (sorted[0] - 1.0, 0.0);
    }
    if target >= 1.0 {
        return (0.0, 1.0);
    }
    // Threshold at the target quantile; escalate when logprob < threshold.
    let idx = ((target * n as f64).floor() as usize).min(n - 1);
    let threshold = sorted[idx];
    let escalate = sorted.iter().filter(|&&v| v < threshold).count();
    (threshold, escalate as f64 / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_sample() {
        assert_eq!(calibrate_threshold(&[], 0.2), (0, 0.0));
    }

    #[test]
    fn test_calibrate_logprob_threshold() {
        // 10 values from -1.0 (worst) to -0.1 (best). Target 30% escalation ->
        // threshold near the 30th percentile; ~30% fall below it.
        let lps: Vec<f64> = (1..=10).map(|i| -(i as f64) / 10.0).collect();
        let (thr, rate) = calibrate_logprob_threshold(&lps, 0.3);
        assert!((-0.8..=-0.6).contains(&thr), "threshold {thr}");
        assert!((0.2..=0.4).contains(&rate), "rate {rate}");
        assert_eq!(calibrate_logprob_threshold(&[], 0.2), (0.0, 0.0));
        assert_eq!(calibrate_logprob_threshold(&lps, 1.0), (0.0, 1.0));
        assert_eq!(calibrate_logprob_threshold(&lps, 0.0).1, 0.0);
    }

    #[test]
    fn test_target_extremes() {
        let t = [10, 20, 30, 40];
        assert_eq!(calibrate_threshold(&t, 1.0), (0, 1.0));
        // Nothing to cloud -> threshold above the max.
        let (thr, rate) = calibrate_threshold(&t, 0.0);
        assert_eq!(thr, 41);
        assert_eq!(rate, 0.0);
    }

    #[test]
    fn test_quantile_threshold() {
        // 100 prompts of sizes 1..=100. Target 20% cloud -> threshold near the
        // 80th percentile, ~20% at or above it.
        let tokens: Vec<u64> = (1..=100).collect();
        let (thr, rate) = calibrate_threshold(&tokens, 0.2);
        assert!((75..=85).contains(&thr), "threshold {thr}");
        assert!((0.15..=0.25).contains(&rate), "rate {rate}");
    }

    #[test]
    fn test_achieved_rate_is_real_fraction() {
        // Clustered sizes: ties mean achieved may not equal target exactly, but
        // it must be the true fraction >= threshold.
        let tokens = [100u64, 100, 100, 100, 500];
        let (thr, rate) = calibrate_threshold(&tokens, 0.2);
        let actual = tokens.iter().filter(|&&t| t >= thr as u64).count() as f64 / 5.0;
        assert!((rate - actual).abs() < 1e-9);
    }

    #[test]
    fn test_clamps_out_of_range_target() {
        let t = [5, 15, 25];
        assert_eq!(calibrate_threshold(&t, 5.0), (0, 1.0));
        assert_eq!(calibrate_threshold(&t, -1.0).1, 0.0);
    }
}

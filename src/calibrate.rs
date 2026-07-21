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
//!
//! When the user *does* have labels — (mean-logprob, was-the-answer-correct)
//! pairs — `ErrorCurve` upgrades the cascade knob from a target-*rate* control
//! to a target-*accuracy* control (IMP-13, UCCI arXiv:2605.18796): a monotone
//! map from mean-logprob to estimated error probability, fit with the Pool
//! Adjacent Violators Algorithm (isotonic regression, std-only). The fit is
//! advisory and degrades gracefully with few labels: fewer labels → coarser
//! blocks → more conservative recommendations.

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
    // tokens >= T is <= target. That requires k = ceil((1-target)*n) so the
    // achieved fraction (n-k)/n never exceeds target. floor() would let the
    // achieved rate exceed the budget when (1-target)*n is non-integer (ADR-109).
    let idx = (((1.0 - target) * n as f64).ceil() as usize).min(n - 1);
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
    // Drop non-finite values before sorting: NaN breaks sort invariants because
    // `partial_cmp` returns None for NaN, and unwrap_or(Equal) treats NaN as equal
    // to every value, producing an unsorted result and wrong quantile thresholds.
    let mut sorted: Vec<f64> = logprobs.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return (0.0, 0.0);
    }
    let n = sorted.len();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("filtered to finite"));

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

/// Standard target rates for `--sweep` reports (IMP-42): enough points to see
/// the whole cost/quality curve in one table without overwhelming the
/// terminal. RouteLLM's operational finding is that there is no universally
/// correct threshold — the right one depends on the user's own traffic and
/// cost tolerance — so showing several candidate rates side by side, instead
/// of one point picked via `--target`, is the actionable form of that advice.
pub const DEFAULT_SWEEP_TARGETS: &[f64] = &[0.05, 0.10, 0.20, 0.30, 0.50];

/// `calibrate_threshold` evaluated at each of `targets` (IMP-42). Returns
/// `(target, threshold, achieved_rate)` triples in `targets` order. Each
/// target is an independent call to the unmodified `calibrate_threshold` (a
/// fresh sort of `tokens` per target) — fine for the cost-log sizes this CLI
/// tool operates on; not a hot path.
pub fn sweep_thresholds(tokens: &[u64], targets: &[f64]) -> Vec<(f64, usize, f64)> {
    targets
        .iter()
        .map(|&target| {
            let (threshold, achieved) = calibrate_threshold(tokens, target);
            (target, threshold, achieved)
        })
        .collect()
}

/// `calibrate_logprob_threshold` evaluated at each of `targets` (IMP-42): the
/// same sweep idea applied to the cascade escalation-rate axis instead of the
/// length-routing axis.
pub fn sweep_logprob_thresholds(logprobs: &[f64], targets: &[f64]) -> Vec<(f64, f64, f64)> {
    targets
        .iter()
        .map(|&target| {
            let (threshold, achieved) = calibrate_logprob_threshold(logprobs, target);
            (target, threshold, achieved)
        })
        .collect()
}

/// A labelled confidence observation (IMP-13): the local model's mean token
/// log-probability for one answer, and whether that answer was judged correct.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabeledLogprob {
    pub logprob: f64,
    pub correct: bool,
}

/// A monotone, non-increasing map from mean-logprob to estimated error
/// probability (IMP-13, UCCI arXiv:2605.18796), fit with the Pool Adjacent
/// Violators Algorithm. Higher confidence (logprob closer to 0) never yields a
/// higher estimated error — the monotonicity constraint is what lets a small
/// label set produce a usable curve instead of noise.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorCurve {
    /// Step blocks ascending by logprob: (lowest logprob in block, estimated
    /// error probability, number of labels pooled into the block). Error values
    /// are non-increasing across blocks.
    blocks: Vec<(f64, f64, usize)>,
}

impl ErrorCurve {
    /// Fit the curve on labelled observations. Non-finite logprobs are dropped;
    /// returns `None` when nothing usable remains.
    pub fn fit(labeled: &[LabeledLogprob]) -> Option<ErrorCurve> {
        let mut pts: Vec<(f64, f64)> = labeled
            .iter()
            .filter(|l| l.logprob.is_finite())
            .map(|l| (l.logprob, if l.correct { 0.0 } else { 1.0 }))
            .collect();
        if pts.is_empty() {
            return None;
        }
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("filtered to finite"));
        // PAVA for a non-increasing fit: walk left to right keeping blocks of
        // (start_logprob, error_sum, count); a later block whose mean exceeds
        // its predecessor's violates monotonicity and is merged into it.
        // Equal means are merged too — same fitted function, canonical minimal
        // blocks (so the advisory printout pools ties into one honest n=).
        let mut blocks: Vec<(f64, f64, usize)> = Vec::new();
        for (lp, y) in pts {
            blocks.push((lp, y, 1));
            while blocks.len() >= 2 {
                let (_, last_sum, last_n) = blocks[blocks.len() - 1];
                let (_, prev_sum, prev_n) = blocks[blocks.len() - 2];
                if prev_sum / (prev_n as f64) <= last_sum / (last_n as f64) {
                    blocks.pop();
                    let prev = blocks.last_mut().expect("len checked >= 2");
                    prev.1 += last_sum;
                    prev.2 += last_n;
                } else {
                    break;
                }
            }
        }
        Some(ErrorCurve {
            blocks: blocks
                .into_iter()
                .map(|(lp, sum, n)| (lp, sum / n as f64, n))
                .collect(),
        })
    }

    /// Estimated error probability at a given mean logprob (step lookup).
    /// Below the first block the first (highest-error) estimate applies.
    pub fn error_at(&self, logprob: f64) -> f64 {
        let mut est = self.blocks[0].1;
        for &(start, err, _) in &self.blocks {
            if start <= logprob {
                est = err;
            } else {
                break;
            }
        }
        est
    }

    /// The lowest observed logprob whose estimated error is `<= target` —
    /// usable directly as `PASTURE_CASCADE_LOGPROB`: answers below it escalate,
    /// answers at or above it stay local with estimated error within budget.
    /// `None` when even the most confident block exceeds the target.
    pub fn threshold_for_error(&self, target: f64) -> Option<f64> {
        self.blocks
            .iter()
            .find(|&&(_, err, _)| err <= target)
            .map(|&(lp, _, _)| lp)
    }

    /// The fitted step curve: `(lowest logprob in block, estimated error, n)`,
    /// ascending by logprob; each estimate applies until the next block starts.
    pub fn breakpoints(&self) -> &[(f64, f64, usize)] {
        &self.blocks
    }
}

/// Load labelled (logprob, correct) pairs from a JSONL file (IMP-13). Each
/// non-blank, non-`//` line must be a JSON object with a finite numeric
/// `"logprob"` and a boolean `"correct"`:
///
/// ```json
/// {"logprob": -0.42, "correct": true}
/// {"logprob": -1.87, "correct": false}
/// ```
///
/// The `logprob` values come from the cost log's `logprob` field (cascade
/// runs record them); `correct` is the user's own judgement of that answer.
pub fn load_labeled_logprobs(path: &str) -> Result<Vec<LabeledLogprob>, String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (lineno, line_res) in reader.lines().enumerate() {
        let line = line_res.map_err(|e| format!("{path}:{}: read error: {e}", lineno + 1))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let val = crate::json::parse(line).map_err(|e| format!("{path}:{}: {e}", lineno + 1))?;
        let logprob = val.get("logprob").and_then(|v| v.as_f64()).ok_or_else(|| {
            format!(
                "{path}:{}: missing or non-numeric 'logprob' field",
                lineno + 1
            )
        })?;
        if !logprob.is_finite() {
            return Err(format!("{path}:{}: 'logprob' must be finite", lineno + 1));
        }
        let correct = val
            .get("correct")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                format!(
                    "{path}:{}: missing or non-boolean 'correct' field",
                    lineno + 1
                )
            })?;
        out.push(LabeledLogprob { logprob, correct });
    }
    Ok(out)
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
    fn test_sweep_thresholds_matches_individual_calls() {
        // IMP-42: a sweep row for target T must equal calibrate_threshold(_, T)
        // exactly — the sweep is a thin loop, not a re-derivation.
        let tokens: Vec<u64> = (1..=100).collect();
        let targets = [0.1, 0.25, 0.5];
        let sweep = sweep_thresholds(&tokens, &targets);
        assert_eq!(sweep.len(), targets.len());
        for (i, &t) in targets.iter().enumerate() {
            let (thr, achieved) = calibrate_threshold(&tokens, t);
            assert_eq!(sweep[i], (t, thr, achieved), "row {i}");
        }
    }

    #[test]
    fn test_sweep_logprob_matches_individual_calls() {
        let lps: Vec<f64> = (1..=10).map(|i| -(i as f64) / 10.0).collect();
        let targets = [0.1, 0.3, 0.5];
        let sweep = sweep_logprob_thresholds(&lps, &targets);
        assert_eq!(sweep.len(), targets.len());
        for (i, &t) in targets.iter().enumerate() {
            let (thr, achieved) = calibrate_logprob_threshold(&lps, t);
            assert_eq!(sweep[i], (t, thr, achieved), "row {i}");
        }
    }

    #[test]
    fn test_sweep_preserves_target_order_and_handles_empty() {
        // Order in == order out (the report renders rows in the caller's order),
        // and an empty sample yields one row per target, none of them panicking.
        let targets = [0.5, 0.1, 0.3];
        let sweep = sweep_thresholds(&[], &targets);
        let out_targets: Vec<f64> = sweep.iter().map(|(t, _, _)| *t).collect();
        assert_eq!(out_targets, targets);
        // Empty sample: calibrate_threshold returns (0, 0.0) for every target.
        assert!(sweep.iter().all(|&(_, thr, rate)| thr == 0 && rate == 0.0));
    }

    #[test]
    fn test_default_sweep_targets_sorted_and_in_range() {
        // The report reads top-to-bottom as "cheaper … pricier"; keep the
        // defaults strictly ascending and inside (0,1) so no row is a degenerate
        // all-local / all-cloud extreme.
        let d = DEFAULT_SWEEP_TARGETS;
        assert!(!d.is_empty());
        assert!(
            d.windows(2).all(|w| w[0] < w[1]),
            "must be strictly ascending"
        );
        assert!(
            d.iter().all(|&t| t > 0.0 && t < 1.0),
            "must be within (0,1)"
        );
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
    fn test_achieved_rate_never_exceeds_target() {
        // ADR-109: when (1-target)*n is non-integer, floor() gave an index that
        // let the achieved rate exceed the target budget (e.g. n=3, target=0.5
        // → floor(1.5)=1 → achieved=2/3=0.667 > 0.5). ceil() fixes this.
        let tokens = [10u64, 20, 30]; // n=3
        let (thr, rate) = calibrate_threshold(&tokens, 0.5);
        let actual = tokens.iter().filter(|&&t| t >= thr as u64).count() as f64 / 3.0;
        assert!(
            actual <= 0.5 + 1e-9,
            "achieved {actual} must not exceed target 0.5 (threshold {thr})"
        );
        assert!(
            (rate - actual).abs() < 1e-9,
            "reported rate must match actual"
        );
    }

    #[test]
    fn test_clamps_out_of_range_target() {
        let t = [5, 15, 25];
        assert_eq!(calibrate_threshold(&t, 5.0), (0, 1.0));
        assert_eq!(calibrate_threshold(&t, -1.0).1, 0.0);
    }

    #[test]
    fn test_logprob_threshold_filters_non_finite() {
        // NaN and inf in the input must be silently dropped before sorting.
        // Prior behaviour: partial_cmp returned None for NaN and was treated as
        // Equal, breaking the sort and producing an incorrect threshold.
        let lps = vec![-0.1, f64::NAN, -0.5, f64::INFINITY, -0.3];
        let (thr, _) = calibrate_logprob_threshold(&lps, 0.5);
        // Three finite values: -0.5, -0.3, -0.1. Sorted: [-0.5, -0.3, -0.1].
        // 50% of 3 → idx 1 → threshold = -0.3.
        assert!(thr.is_finite(), "threshold must be finite after NaN filter");
        // All-NaN input must return the safe (0.0, 0.0) default.
        let all_nan = vec![f64::NAN, f64::NAN];
        assert_eq!(calibrate_logprob_threshold(&all_nan, 0.5), (0.0, 0.0));
    }

    // ── ErrorCurve (IMP-13) tests ────────────────────────────────────────────

    fn lab(logprob: f64, correct: bool) -> LabeledLogprob {
        LabeledLogprob { logprob, correct }
    }

    #[test]
    fn test_error_curve_pava_merges_violators() {
        // Errors by ascending logprob: 1,1,0,1,0,0. The 0-then-1 at positions
        // 3..4 violates non-increasing → PAVA pools them into a 0.5 block.
        let labeled = vec![
            lab(-3.0, false),
            lab(-2.5, false),
            lab(-2.0, true),
            lab(-1.5, false),
            lab(-1.0, true),
            lab(-0.5, true),
        ];
        let curve = ErrorCurve::fit(&labeled).unwrap();
        let bps = curve.breakpoints();
        assert_eq!(bps.len(), 3, "blocks: {bps:?}");
        assert_eq!(bps[0], (-3.0, 1.0, 2));
        assert_eq!(bps[1], (-2.0, 0.5, 2));
        assert_eq!(bps[2], (-1.0, 0.0, 2));
    }

    #[test]
    fn test_error_curve_fitted_values_non_increasing() {
        // Alternating correctness: the fit must still be monotone.
        let labeled: Vec<LabeledLogprob> = (0..20)
            .map(|i| lab(-2.0 + 0.1 * i as f64, i % 3 != 0))
            .collect();
        let curve = ErrorCurve::fit(&labeled).unwrap();
        let bps = curve.breakpoints();
        for w in bps.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "error estimates must be non-increasing: {bps:?}"
            );
        }
        // Pooled counts must account for every label.
        assert_eq!(bps.iter().map(|b| b.2).sum::<usize>(), 20);
    }

    #[test]
    fn test_error_curve_error_at_lookup() {
        let labeled = vec![
            lab(-3.0, false),
            lab(-2.5, false),
            lab(-2.0, true),
            lab(-1.5, false),
            lab(-1.0, true),
            lab(-0.5, true),
        ];
        let curve = ErrorCurve::fit(&labeled).unwrap();
        // Below the first block → first (worst) estimate.
        assert_eq!(curve.error_at(-10.0), 1.0);
        // Inside each block → that block's estimate.
        assert_eq!(curve.error_at(-2.6), 1.0);
        assert_eq!(curve.error_at(-1.7), 0.5);
        assert_eq!(curve.error_at(-0.2), 0.0);
    }

    #[test]
    fn test_error_curve_threshold_for_error() {
        let labeled = vec![
            lab(-3.0, false),
            lab(-2.5, false),
            lab(-2.0, true),
            lab(-1.5, false),
            lab(-1.0, true),
            lab(-0.5, true),
        ];
        let curve = ErrorCurve::fit(&labeled).unwrap();
        // Blocks: (-3.0, 1.0), (-2.0, 0.5), (-1.0, 0.0).
        assert_eq!(curve.threshold_for_error(0.6), Some(-2.0));
        assert_eq!(curve.threshold_for_error(0.05), Some(-1.0));
        // Target met even by the worst block → lowest logprob (escalate ~nothing).
        assert_eq!(curve.threshold_for_error(1.0), Some(-3.0));
    }

    #[test]
    fn test_error_curve_unachievable_target() {
        // Every answer wrong → single block at error 1.0; no threshold meets 50%.
        let labeled = vec![lab(-2.0, false), lab(-1.0, false), lab(-0.5, false)];
        let curve = ErrorCurve::fit(&labeled).unwrap();
        assert_eq!(curve.breakpoints(), &[(-2.0, 1.0, 3)]);
        assert_eq!(curve.threshold_for_error(0.5), None);
    }

    #[test]
    fn test_error_curve_all_correct() {
        let labeled = vec![lab(-2.0, true), lab(-1.0, true)];
        let curve = ErrorCurve::fit(&labeled).unwrap();
        assert_eq!(curve.breakpoints(), &[(-2.0, 0.0, 2)]);
        // Any budget is met from the lowest observed logprob.
        assert_eq!(curve.threshold_for_error(0.01), Some(-2.0));
    }

    #[test]
    fn test_error_curve_filters_non_finite_and_empty() {
        assert!(ErrorCurve::fit(&[]).is_none());
        assert!(ErrorCurve::fit(&[lab(f64::NAN, true)]).is_none());
        let curve = ErrorCurve::fit(&[lab(f64::NAN, false), lab(-1.0, true)]).unwrap();
        assert_eq!(curve.breakpoints().len(), 1);
        assert_eq!(curve.error_at(-1.0), 0.0);
    }

    fn tmp_labels(name: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        let p = std::env::temp_dir().join(format!("pasture_labels_{name}.jsonl"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn test_load_labeled_logprobs_basic() {
        let p = tmp_labels(
            "basic",
            "{\"logprob\": -0.42, \"correct\": true}\n\n// comment\n{\"logprob\": -1.87, \"correct\": false}\n",
        );
        let labels = load_labeled_logprobs(p.to_str().unwrap()).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0], lab(-0.42, true));
        assert_eq!(labels[1], lab(-1.87, false));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_load_labeled_logprobs_rejects_bad_lines() {
        for (name, body, want) in [
            ("missing_lp", "{\"correct\": true}\n", "logprob"),
            ("missing_c", "{\"logprob\": -0.5}\n", "correct"),
            (
                "string_c",
                "{\"logprob\": -0.5, \"correct\": \"yes\"}\n",
                "correct",
            ),
            ("not_json", "not json\n", ""),
        ] {
            let p = tmp_labels(name, body);
            let err = load_labeled_logprobs(p.to_str().unwrap()).unwrap_err();
            assert!(
                err.contains(":1:"),
                "{name}: error must cite the line: {err}"
            );
            assert!(err.contains(want), "{name}: {err}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn test_load_labeled_logprobs_missing_file() {
        assert!(load_labeled_logprobs("/no/such/labels.jsonl").is_err());
    }
}

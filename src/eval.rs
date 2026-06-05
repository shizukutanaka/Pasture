//! Routing evaluation harness (IMP-5, RouterBench-style, arXiv:2403 / Hu 2024).
//!
//! Offline, deterministic, zero-dependency. Measures routing quality against a
//! labelled set (correctness) and sweeps the token threshold to show the
//! cost/locality trade-off. This answers the "brittle hand-tuned rules"
//! critique by making the rules measurable (§6.5: measure before optimising).

use crate::privacy::classify;
use crate::routing::{Route, RoutingEngine};

/// A labelled evaluation case: a prompt and the route it *should* take.
#[derive(Debug, Clone, Copy)]
pub struct EvalCase {
    pub prompt: &'static str,
    pub expected: Route,
}

/// Aggregate result of running the labelled set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalReport {
    pub total: usize,
    pub correct: usize,
    pub cloud_count: usize,
    /// expected Local but routed Cloud (wasted cloud cost).
    pub false_escalations: usize,
    /// expected Cloud but routed Local (quality risk).
    pub missed_escalations: usize,
}

impl EvalReport {
    pub fn accuracy(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.correct as f64 / self.total as f64
        }
    }

    pub fn cloud_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.cloud_count as f64 / self.total as f64
        }
    }
}

/// Run the engine over labelled cases and tally correctness. Sensitivity is
/// derived per case via the privacy classifier (so sensitive -> Local).
pub fn run_eval(engine: &RoutingEngine, cases: &[EvalCase]) -> EvalReport {
    let mut report = EvalReport {
        total: cases.len(),
        correct: 0,
        cloud_count: 0,
        false_escalations: 0,
        missed_escalations: 0,
    };
    for case in cases {
        let sensitive = classify(case.prompt).is_sensitive();
        let got = engine
            .decide_with_sensitivity(case.prompt, None, sensitive)
            .map(|d| d.route)
            .unwrap_or(Route::Local);
        if got == Route::Cloud {
            report.cloud_count += 1;
        }
        if got == case.expected {
            report.correct += 1;
        } else if case.expected == Route::Local && got == Route::Cloud {
            report.false_escalations += 1;
        } else {
            report.missed_escalations += 1;
        }
    }
    report
}

/// The built-in labelled dataset (EN + JA): plain->Local, hard->Cloud,
/// sensitive->Local. Designed to be threshold-independent so it serves as a
/// routing-correctness regression check.
pub fn default_cases() -> Vec<EvalCase> {
    use Route::{Cloud, Local};
    vec![
        // Plain / short factual -> Local
        EvalCase {
            prompt: "what is the capital of France",
            expected: Local,
        },
        EvalCase {
            prompt: "translate good morning to french",
            expected: Local,
        },
        EvalCase {
            prompt: "list three primary colors",
            expected: Local,
        },
        EvalCase {
            prompt: "give a synonym for fast",
            expected: Local,
        },
        EvalCase {
            prompt: "好きな食べ物は何",
            expected: Local,
        },
        // Hard (reasoning / format / code / math / multi-question) -> Cloud
        EvalCase {
            prompt: "prove that the square root of two is irrational, step by step",
            expected: Cloud,
        },
        EvalCase {
            prompt: "write a function to reverse a string",
            expected: Cloud,
        },
        EvalCase {
            prompt: "return the result as JSON",
            expected: Cloud,
        },
        EvalCase {
            prompt: "why? how? when?",
            expected: Cloud,
        },
        EvalCase {
            prompt: "compute x = a + b * c / d ^ 2",
            expected: Cloud,
        },
        EvalCase {
            prompt: "```js\nconsole.log(1)\n```",
            expected: Cloud,
        },
        EvalCase {
            prompt: "これを順を追って証明して",
            expected: Cloud,
        },
        // Sensitive -> Local (privacy overrides)
        EvalCase {
            prompt: "email alice@example.com the invoice",
            expected: Local,
        },
        EvalCase {
            prompt: "my password is correcthorse",
            expected: Local,
        },
        EvalCase {
            prompt: "card 4111 1111 1111 1111",
            expected: Local,
        },
        EvalCase {
            prompt: "key sk-abcdefghijklmnop1234",
            expected: Local,
        },
        EvalCase {
            prompt: "ping 192.168.1.1",
            expected: Local,
        },
        EvalCase {
            prompt: "phone +1-202-555-0143",
            expected: Local,
        },
    ]
}

/// Plain prompts of increasing length (no hard signals, not sensitive) used to
/// visualise how the token threshold trades locality against cost.
pub fn length_samples() -> Vec<String> {
    let unit = "the quiet meadow at dawn ";
    [1usize, 10, 60, 200]
        .iter()
        .map(|n| unit.repeat(*n).trim_end().to_string())
        .collect()
}

/// Sweep token thresholds, returning (threshold, cloud_rate) over the samples.
pub fn sweep(samples: &[String], thresholds: &[usize]) -> Vec<(usize, f64)> {
    thresholds
        .iter()
        .map(|&t| {
            let engine = RoutingEngine::new(t, true, true);
            let cloud = samples
                .iter()
                .filter(|s| {
                    engine
                        .decide(s, None)
                        .map(|d| d.route == Route::Cloud)
                        .unwrap_or(false)
                })
                .count();
            let rate = if samples.is_empty() {
                0.0
            } else {
                cloud as f64 / samples.len() as f64
            };
            (t, rate)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_cases_route_correctly() {
        // Use a CPU-ish threshold; the set is threshold-independent.
        let engine = RoutingEngine::new(300, true, true);
        let report = run_eval(&engine, &default_cases());
        assert_eq!(
            report.correct, report.total,
            "expected 100% accuracy, got {}/{} (false_esc={}, missed_esc={})",
            report.correct, report.total, report.false_escalations, report.missed_escalations
        );
    }

    #[test]
    fn test_accuracy_and_cloud_rate_math() {
        let r = EvalReport {
            total: 4,
            correct: 3,
            cloud_count: 2,
            false_escalations: 1,
            missed_escalations: 0,
        };
        assert!((r.accuracy() - 0.75).abs() < 1e-9);
        assert!((r.cloud_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_empty_report() {
        let r = EvalReport {
            total: 0,
            correct: 0,
            cloud_count: 0,
            false_escalations: 0,
            missed_escalations: 0,
        };
        assert_eq!(r.accuracy(), 0.0);
        assert_eq!(r.cloud_rate(), 0.0);
    }

    #[test]
    fn test_sweep_monotonic_nonincreasing_with_threshold() {
        let samples = length_samples();
        let res = sweep(&samples, &[50, 100, 300, 800, 5000]);
        // Higher threshold -> fewer cloud routings (rate is non-increasing).
        for w in res.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "cloud rate should not increase with threshold: {res:?}"
            );
        }
        // Lowest threshold should push at least one sample to cloud.
        assert!(res[0].1 > 0.0);
    }

    #[test]
    fn test_length_samples_not_sensitive_and_no_hard_signal() {
        for s in length_samples() {
            assert!(!classify(&s).is_sensitive());
            assert!(crate::routing::hard_signals(&s).is_empty());
        }
    }
}

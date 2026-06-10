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

    /// Machine-readable summary for `pasture eval --json` (CI / scripting).
    pub fn to_json(&self, threshold: usize) -> String {
        format!(
            "{{\"total\":{},\"correct\":{},\"accuracy\":{:.6},\"cloud_count\":{},\"cloud_rate\":{:.6},\"false_escalations\":{},\"missed_escalations\":{},\"threshold\":{}}}",
            self.total,
            self.correct,
            self.accuracy(),
            self.cloud_count,
            self.cloud_rate(),
            self.false_escalations,
            self.missed_escalations,
            threshold
        )
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

/// A dynamically-loaded evaluation case (prompt owned on the heap, not `'static`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEvalCase {
    pub prompt: String,
    pub expected: Route,
}

/// Run the engine over dynamically-loaded eval cases. Identical routing logic
/// to `run_eval`; a separate type is used because loaded prompts are not `'static`.
pub fn run_eval_owned(engine: &RoutingEngine, cases: &[OwnedEvalCase]) -> EvalReport {
    let mut report = EvalReport {
        total: cases.len(),
        correct: 0,
        cloud_count: 0,
        false_escalations: 0,
        missed_escalations: 0,
    };
    for case in cases {
        let sensitive = classify(case.prompt.as_str()).is_sensitive();
        let got = engine
            .decide_with_sensitivity(case.prompt.as_str(), None, sensitive)
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

/// Load a JSONL eval file (RouterBench-compatible format). Each non-blank,
/// non-comment line must be a JSON object with a `"prompt"` string and an
/// `"expected"` field that is `"local"` or `"cloud"`. Lines starting with `//`
/// are skipped.
///
/// # Format
/// ```json
/// {"prompt": "what is 2+2", "expected": "local"}
/// {"prompt": "prove the halting problem", "expected": "cloud"}
/// ```
pub fn load_eval_cases(path: &str) -> Result<Vec<OwnedEvalCase>, String> {
    use std::io::{BufRead, BufReader};
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let reader = BufReader::new(file);
    let mut cases = Vec::new();
    for (lineno, line_res) in reader.lines().enumerate() {
        let line = line_res.map_err(|e| format!("{path}:{}: read error: {e}", lineno + 1))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let val = crate::json::parse(line)
            .map_err(|e| format!("{path}:{}: {e}", lineno + 1))?;
        let prompt = val
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!("{path}:{}: missing or non-string 'prompt' field", lineno + 1)
            })?
            .to_string();
        if prompt.is_empty() {
            return Err(format!("{path}:{}: prompt must not be empty", lineno + 1));
        }
        let expected_str = val
            .get("expected")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!("{path}:{}: missing or non-string 'expected' field", lineno + 1)
            })?;
        let expected = match expected_str {
            "local" => Route::Local,
            "cloud" => Route::Cloud,
            other => {
                return Err(format!(
                    "{path}:{}: expected 'local' or 'cloud', got {:?}",
                    lineno + 1,
                    other
                ))
            }
        };
        cases.push(OwnedEvalCase { prompt, expected });
    }
    Ok(cases)
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
    fn test_report_to_json_roundtrips() {
        let r = EvalReport {
            total: 18,
            correct: 18,
            cloud_count: 7,
            false_escalations: 0,
            missed_escalations: 0,
        };
        let json = r.to_json(300);
        let v = crate::json::parse(&json).expect("eval --json must be valid JSON");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(18.0));
        assert_eq!(v.get("correct").and_then(|x| x.as_f64()), Some(18.0));
        assert_eq!(v.get("threshold").and_then(|x| x.as_f64()), Some(300.0));
        assert_eq!(v.get("missed_escalations").and_then(|x| x.as_f64()), Some(0.0));
        assert!((v.get("accuracy").and_then(|x| x.as_f64()).unwrap() - 1.0).abs() < 1e-9);
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

    fn tmp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pasture_eval_{name}.jsonl"))
    }

    #[test]
    fn test_load_eval_cases_basic() {
        use std::io::Write;
        let p = tmp_path("basic");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"prompt":"hi","expected":"local"}}"#).unwrap();
        writeln!(f, r#"{{"prompt":"prove sqrt(2) irrational","expected":"cloud"}}"#).unwrap();
        let cases = load_eval_cases(p.to_str().unwrap()).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].prompt, "hi");
        assert_eq!(cases[0].expected, Route::Local);
        assert_eq!(cases[1].prompt, "prove sqrt(2) irrational");
        assert_eq!(cases[1].expected, Route::Cloud);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_load_eval_cases_skips_blank_and_comment_lines() {
        use std::io::Write;
        let p = tmp_path("skip");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f).unwrap();
        writeln!(f, "// this is a comment").unwrap();
        writeln!(f, r#"{{"prompt":"hello","expected":"local"}}"#).unwrap();
        writeln!(f).unwrap();
        let cases = load_eval_cases(p.to_str().unwrap()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].prompt, "hello");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_load_eval_cases_rejects_invalid_expected() {
        use std::io::Write;
        let p = tmp_path("bad_expected");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"prompt":"hi","expected":"neither"}}"#).unwrap();
        assert!(load_eval_cases(p.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_load_eval_cases_empty_file() {
        let p = tmp_path("empty");
        std::fs::File::create(&p).unwrap();
        let cases = load_eval_cases(p.to_str().unwrap()).unwrap();
        assert!(cases.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_run_eval_owned_routes_correctly() {
        let engine = RoutingEngine::new(300, true, true);
        let cases = vec![
            OwnedEvalCase {
                prompt: "what is the capital of France".to_string(),
                expected: Route::Local,
            },
            OwnedEvalCase {
                prompt: "prove that the square root of two is irrational, step by step"
                    .to_string(),
                expected: Route::Cloud,
            },
        ];
        let report = run_eval_owned(&engine, &cases);
        assert_eq!(report.total, 2);
        assert_eq!(report.correct, 2, "run_eval_owned should match built-in routing");
    }
}

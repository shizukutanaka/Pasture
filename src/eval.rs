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
    /// prompts the privacy classifier marked sensitive that routed Cloud
    /// anyway (ADR-271). Any non-zero value is an I2 breach and `pasture eval`
    /// exits 1 on it unconditionally.
    pub sensitive_escalations: usize,
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
            "{{\"total\":{},\"correct\":{},\"accuracy\":{:.6},\"cloud_count\":{},\"cloud_rate\":{:.6},\"false_escalations\":{},\"missed_escalations\":{},\"sensitive_escalations\":{},\"threshold\":{}}}",
            self.total,
            self.correct,
            self.accuracy(),
            self.cloud_count,
            self.cloud_rate(),
            self.false_escalations,
            self.missed_escalations,
            self.sensitive_escalations,
            threshold
        )
    }
}

/// Score `(prompt, expected)` pairs — the single scorer behind both the
/// static and owned case types (ADR-271; they were the same 20 lines twice,
/// and the held-out corpus would have made it three).
fn score<'a>(engine: &RoutingEngine, cases: impl Iterator<Item = (&'a str, Route)>) -> EvalReport {
    let mut report = EvalReport {
        total: 0,
        correct: 0,
        cloud_count: 0,
        false_escalations: 0,
        missed_escalations: 0,
        sensitive_escalations: 0,
    };
    for (prompt, expected) in cases {
        report.total += 1;
        let sensitive = classify(prompt).is_sensitive();
        let got = engine
            .decide_with_sensitivity(prompt, None, sensitive)
            .map(|d| d.route)
            .unwrap_or(Route::Local);
        if got == Route::Cloud {
            report.cloud_count += 1;
            if sensitive {
                report.sensitive_escalations += 1;
            }
        }
        if got == expected {
            report.correct += 1;
        } else if expected == Route::Local && got == Route::Cloud {
            report.false_escalations += 1;
        } else {
            report.missed_escalations += 1;
        }
    }
    report
}

/// Run the engine over labelled cases and tally correctness. Sensitivity is
/// derived per case via the privacy classifier (so sensitive -> Local).
pub fn run_eval(engine: &RoutingEngine, cases: &[EvalCase]) -> EvalReport {
    score(engine, cases.iter().map(|c| (c.prompt, c.expected)))
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

/// A labelled cascade case: a local model's answer and whether the cascade
/// *should* escalate it to the cloud.
#[derive(Debug, Clone, Copy)]
pub struct CascadeCase {
    pub answer: &'static str,
    pub should_escalate: bool,
}

/// Held-out floor for the cascade answer classifier (ADR-272). **Measured, not
/// chosen**: `is_low_confidence` scored **47.8% (11/23)** on first measurement,
/// catching **0 of 12** held-out weak answers with 0 false escalations.
///
/// Read that carefully — a classifier that always returned `false` scores the
/// same 11/23 (the corpus is 11 keep / 12 escalate). On answers that do not
/// contain its own literal marker strings, the text heuristic contributes no
/// signal at all.
///
/// **Which means this floor is, today, near-vacuous, and that is stated rather
/// than hidden.** It was mutation-tested: forcing `is_low_confidence` to return
/// `false` unconditionally does NOT trip it, precisely because the classifier
/// already scores what "always false" scores. The assertions with real teeth
/// are the companion `false_escalations == 0` (verified: widening the
/// short-answer rule to 40 chars trips it) and the marker guard. The floor
/// becomes a meaningful ratchet the moment the classifier improves and the
/// number is raised.
pub const CASCADE_HOLDOUT_FLOOR: f64 = 0.47;

/// Held-out corpus for `cascade::is_low_confidence` (ADR-272).
///
/// ADR-271 found the routing corpus was a tautology; the cascade classifier had
/// the same problem one layer down — its unit tests feed it its own
/// `UNCERTAINTY_MARKERS` strings. These answers are ones a small local model
/// plausibly produces and **contain no marker substring**, a property enforced
/// by `test_cascade_holdout_contains_no_uncertainty_marker`.
///
/// Two classes: hedging non-answers that should escalate, and terse-but-correct
/// answers that must not (escalating those is wasted cloud spend — the failure
/// direction a naive "short answer means weak" rule would cause).
///
/// As with the routing corpus: a failure here is a finding about the
/// classifier, not a prompt to reword.
pub fn cascade_holdout_cases() -> Vec<CascadeCase> {
    vec![
        // Weak / evasive answers -> should escalate.
        CascadeCase {
            answer: "That depends on a lot of factors, and reasonable people disagree.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "There are several ways to look at this question.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "It is complicated and there is no single right answer.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "Great question! Let me think about what would be best here.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "The answer varies depending on your specific situation.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "You should probably consult a professional about this.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "Well, that is certainly something worth considering carefully.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "The heat pump versus furnace question has many considerations to weigh.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "It really comes down to your own preferences and circumstances.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "Hmm, this is the kind of thing that requires more context to say.",
            should_escalate: true,
        },
        CascadeCase {
            answer: "難しい質問ですね。状況によって答えは変わります。",
            should_escalate: true,
        },
        CascadeCase {
            answer: "一概には言えません。人それぞれだと思います。",
            should_escalate: true,
        },
        // Terse but correct -> must NOT escalate.
        CascadeCase {
            answer: "Paris.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "The chemical symbol for gold is Au.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Eleven players per side.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Green.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Frank Herbert wrote Dune in 1965.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Python is interpreted, though it compiles to bytecode first.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Application Programming Interface.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "The Indian Ocean.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "Seven.",
            should_escalate: false,
        },
        CascadeCase {
            answer: "東京です。",
            should_escalate: false,
        },
        CascadeCase {
            answer: "木星です。",
            should_escalate: false,
        },
    ]
}

/// Score the cascade answer classifier over labelled cases (ADR-272).
/// `false_escalations` = escalated a good answer (wasted spend);
/// `missed_escalations` = kept a weak answer (quality risk).
pub fn run_cascade_eval(cases: &[CascadeCase]) -> EvalReport {
    let mut report = EvalReport {
        total: cases.len(),
        correct: 0,
        cloud_count: 0,
        false_escalations: 0,
        missed_escalations: 0,
        sensitive_escalations: 0,
    };
    for case in cases {
        let got = crate::cascade::is_low_confidence(case.answer);
        if got {
            report.cloud_count += 1;
        }
        if got == case.should_escalate {
            report.correct += 1;
        } else if got {
            report.false_escalations += 1;
        } else {
            report.missed_escalations += 1;
        }
    }
    report
}

/// A dynamically-loaded evaluation case (prompt owned on the heap, not `'static`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEvalCase {
    pub prompt: String,
    pub expected: Route,
}

/// Run the engine over dynamically-loaded eval cases (same scorer as
/// `run_eval`; a separate entry point only because loaded prompts are not
/// `'static`).
pub fn run_eval_owned(engine: &RoutingEngine, cases: &[OwnedEvalCase]) -> EvalReport {
    score(
        engine,
        cases.iter().map(|c| (c.prompt.as_str(), c.expected)),
    )
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
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let reader = BufReader::new(file);
    let mut cases = Vec::new();
    for (lineno, line_res) in reader.lines().enumerate() {
        let line = line_res.map_err(|e| format!("{path}:{}: read error: {e}", lineno + 1))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let val = crate::json::parse(line).map_err(|e| format!("{path}:{}: {e}", lineno + 1))?;
        let prompt = val
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!(
                    "{path}:{}: missing or non-string 'prompt' field",
                    lineno + 1
                )
            })?
            .to_string();
        if prompt.is_empty() {
            return Err(format!("{path}:{}: prompt must not be empty", lineno + 1));
        }
        let expected_str = val
            .get("expected")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!(
                    "{path}:{}: missing or non-string 'expected' field",
                    lineno + 1
                )
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

/// Held-out accuracy floor for `pasture eval` (ADR-271). **Measured, not
/// chosen**: on first measurement the router scored 20/30 (66.7%) on
/// `holdout_cases` — every miss a hard-but-plainly-phrased prompt kept local —
/// and the floor is set one case below that so any regression of a single case
/// turns the gate red. Raise it when routing genuinely improves; lowering it
/// requires a written justification in the ledger.
pub const HOLDOUT_ACCURACY_FLOOR: f64 = 0.66;

/// The held-out generalisation set (ADR-271). Unlike `default_cases` — whose
/// labels restate the router's own marker lists and therefore score 100% by
/// construction — these prompts are labelled by SPEC §4's stated policy
/// (multi-step reasoning / synthesis / open-ended writing → cloud; short
/// factual → local; sensitive → local) while containing **no trigger string
/// from any marker list**, a property enforced by
/// `test_holdout_prompts_contain_no_detector_marker`. All prompts are short, so
/// results are identical for any threshold ≥ 50 and machine-independent.
///
/// When one of these fails, that is a *finding about the router*, not about the
/// prompt. Do not reword a failing prompt into passing — that reintroduces the
/// tautology this set exists to break.
pub fn holdout_cases() -> Vec<EvalCase> {
    use Route::{Cloud, Local};
    vec![
        // Hard, plainly phrased -> Cloud. Multi-step reasoning or open-ended
        // synthesis with no cue word; probes missed escalations.
        EvalCase {
            prompt: "A train leaves Boston at noon going sixty miles an hour and another leaves New York an hour later going forty; where do they meet",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Our team missed the deadline again and morale is low; what should I say in the retrospective",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Which costs less over ten years, a heat pump or a gas furnace, for a drafty house in a cold climate",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Rewrite the opening of my cover letter so it sounds confident without sounding arrogant",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Explain the difference between correlation and causation to a manager who thinks statistics are a waste of time",
            expected: Cloud,
        },
        EvalCase {
            prompt: "My sourdough starter smells like acetone and never doubles in size; what is going wrong",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Design a seating chart for a wedding where two families are not on speaking terms",
            expected: Cloud,
        },
        EvalCase {
            prompt: "Argue the strongest case against the position you would normally take on remote work",
            expected: Cloud,
        },
        EvalCase {
            prompt: "この契約書の条項が借り手に不利かどうか判断して",
            expected: Cloud,
        },
        EvalCase {
            prompt: "何年も弾いていない人が半年で発表会に出られるようにするピアノの練習計画を立てて",
            expected: Cloud,
        },
        // Easy, detector-adjacent -> Local. Trivial asks that brush a
        // detector's vocabulary without being work; probes false escalations.
        EvalCase {
            prompt: "What does the acronym API stand for",
            expected: Local,
        },
        EvalCase {
            prompt: "What does the abbreviation JSON stand for",
            expected: Local,
        },
        EvalCase {
            prompt: "Is Python a compiled language or an interpreted one",
            expected: Local,
        },
        EvalCase {
            prompt: "Which company originally created the Java language",
            expected: Local,
        },
        EvalCase {
            prompt: "What is the chemical symbol for gold",
            expected: Local,
        },
        EvalCase {
            prompt: "How many players are on a soccer team",
            expected: Local,
        },
        EvalCase {
            prompt: "What colour do you get by mixing blue and yellow",
            expected: Local,
        },
        EvalCase {
            prompt: "Who wrote the novel Dune",
            expected: Local,
        },
        EvalCase {
            prompt: "How many continents are there",
            expected: Local,
        },
        EvalCase {
            prompt: "What is the tallest mountain in the world",
            expected: Local,
        },
        EvalCase {
            prompt: "Which ocean lies between Africa and Australia",
            expected: Local,
        },
        EvalCase {
            prompt: "日本の首都はどこ",
            expected: Local,
        },
        EvalCase {
            prompt: "いちばん大きい惑星は何",
            expected: Local,
        },
        // Sensitive, obliquely phrased -> Local. PII in casual phrasings the
        // value detectors were not written against; a Cloud route here is an
        // I2 breach (sensitive_escalations).
        EvalCase {
            prompt: "Send the receipt to maria.lopez@contoso.co.uk when you get a chance",
            expected: Local,
        },
        EvalCase {
            prompt: "The box sitting at 10.0.14.7 keeps dropping connections overnight",
            expected: Local,
        },
        EvalCase {
            prompt: "Reach me on +1-415-555-0132 after six",
            expected: Local,
        },
        EvalCase {
            prompt: "Put it on the card 5555 5555 5555 4444 and mail the receipt",
            expected: Local,
        },
        EvalCase {
            prompt: "the password for the shared archive is hunter2hunter2",
            expected: Local,
        },
        EvalCase {
            prompt: "私のメールアドレスは tanaka@example.jp です",
            expected: Local,
        },
        EvalCase {
            prompt: "the token sk-9fQ2mZx7Lp4Rt8Vw1Nb3 is in the config",
            expected: Local,
        },
    ]
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
            sensitive_escalations: 0,
        };
        let json = r.to_json(300);
        let v = crate::json::parse(&json).expect("eval --json must be valid JSON");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(18.0));
        assert_eq!(v.get("correct").and_then(|x| x.as_f64()), Some(18.0));
        assert_eq!(v.get("threshold").and_then(|x| x.as_f64()), Some(300.0));
        assert_eq!(
            v.get("missed_escalations").and_then(|x| x.as_f64()),
            Some(0.0)
        );
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
            sensitive_escalations: 0,
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
            sensitive_escalations: 0,
        };
        assert_eq!(r.accuracy(), 0.0);
        assert_eq!(r.cloud_rate(), 0.0);
    }

    #[test]
    fn test_cascade_holdout_contains_no_uncertainty_marker() {
        // ADR-272, mirroring the routing corpus guard: these answers only
        // measure generalisation while they share no literal marker with
        // `is_low_confidence`. Markers come from cascade.rs itself, so adding
        // one automatically re-screens this corpus.
        let markers = crate::cascade::uncertainty_markers();
        for case in cascade_holdout_cases() {
            let lower = case.answer.to_lowercase();
            for m in markers {
                assert!(
                    !lower.contains(m),
                    "held-out cascade answer contains marker {m:?}: {:?}",
                    case.answer
                );
            }
            assert!(
                case.answer.trim().chars().count() >= 2,
                "near-empty answers escalate by a separate rule, not the text heuristic: {:?}",
                case.answer
            );
        }
    }

    #[test]
    fn test_cascade_holdout_meets_floor() {
        // Pins the measured baseline. Honest limitation, also recorded on
        // CASCADE_HOLDOUT_FLOOR: because the classifier already scores exactly
        // what a constant `false` scores, this floor cannot detect the
        // classifier being disabled — only the *waste* direction below it. The
        // `false_escalations` assertion underneath is the one with teeth until
        // the classifier improves.
        let r = run_cascade_eval(&cascade_holdout_cases());
        assert!(
            r.accuracy() >= CASCADE_HOLDOUT_FLOOR,
            "cascade held-out accuracy {:.3} fell below the floor {CASCADE_HOLDOUT_FLOOR}: {}/{} (false_esc={}, missed_esc={})",
            r.accuracy(), r.correct, r.total, r.false_escalations, r.missed_escalations
        );
        // Escalating a good short answer is wasted cloud spend; the current
        // classifier never does it, and that should not silently change.
        assert_eq!(
            r.false_escalations, 0,
            "the classifier began escalating correct short answers"
        );
    }

    #[test]
    fn test_holdout_prompts_contain_no_detector_marker() {
        // ADR-271, the load-bearing property: the held-out corpus is only a
        // measurement of generalisation while its prompts share no trigger
        // string with the router. This is what stops the set decaying back
        // into a self-graded exam the first time someone "fixes" a failing
        // prompt by (accidentally or not) pasting a marker into it. Marker
        // lists are pulled from routing.rs itself, so adding a marker there
        // automatically re-screens the corpus here.
        let markers = crate::routing::all_markers();
        for case in holdout_cases() {
            let lower = case.prompt.to_lowercase();
            for m in &markers {
                assert!(
                    !lower.contains(m),
                    "held-out prompt contains detector marker {m:?}: {:?}",
                    case.prompt
                );
            }
            assert!(
                !crate::routing::looks_mathy(case.prompt),
                "held-out prompt trips the math-density signal: {:?}",
                case.prompt
            );
        }
    }

    #[test]
    fn test_holdout_accuracy_meets_floor_and_no_i2_breach() {
        // Pins the measured baseline the CLI gate ratchets against, on a fixed
        // threshold so the result is machine-independent (all holdout prompts
        // are far below 300 estimated tokens). If routing changes make this
        // fail, that is a real regression on prompts with no marker to lean
        // on — fix the routing or justify moving HOLDOUT_ACCURACY_FLOOR in the
        // ledger; never reword the prompt.
        let engine = RoutingEngine::new(300, true, true);
        let report = run_eval(&engine, &holdout_cases());
        assert!(
            report.accuracy() >= HOLDOUT_ACCURACY_FLOOR,
            "held-out accuracy {:.3} fell below the floor {HOLDOUT_ACCURACY_FLOOR}: {}/{} (false_esc={}, missed_esc={})",
            report.accuracy(),
            report.correct,
            report.total,
            report.false_escalations,
            report.missed_escalations
        );
        assert_eq!(
            report.sensitive_escalations, 0,
            "I2 breach: a sensitive held-out prompt routed to cloud"
        );
    }

    #[test]
    fn test_holdout_sensitive_prompts_are_detected() {
        // The oblique-PII class only probes I2 if the classifier actually
        // recognises each prompt as sensitive; silently losing detection would
        // make sensitive_escalations vacuously zero. Every case past the
        // sensitive-class boundary must classify sensitive.
        let sensitive_class: Vec<_> = holdout_cases()
            .into_iter()
            .filter(|c| {
                c.prompt.contains("maria.lopez")
                    || c.prompt.contains("10.0.14.7")
                    || c.prompt.contains("+1-415")
                    || c.prompt.contains("5555 5555")
                    || c.prompt.contains("hunter2")
                    || c.prompt.contains("tanaka@")
                    || c.prompt.contains("sk-9fQ2")
            })
            .collect();
        assert_eq!(sensitive_class.len(), 7, "sensitive class shrank");
        for c in &sensitive_class {
            assert!(
                classify(c.prompt).is_sensitive(),
                "oblique PII no longer detected: {:?}",
                c.prompt
            );
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
        writeln!(
            f,
            r#"{{"prompt":"prove sqrt(2) irrational","expected":"cloud"}}"#
        )
        .unwrap();
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
                prompt: "prove that the square root of two is irrational, step by step".to_string(),
                expected: Route::Cloud,
            },
        ];
        let report = run_eval_owned(&engine, &cases);
        assert_eq!(report.total, 2);
        assert_eq!(
            report.correct, 2,
            "run_eval_owned should match built-in routing"
        );
    }
}

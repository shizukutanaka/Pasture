//! Self-improvement ledger (IMP-13).
//!
//! Pasture's improvement history used to live only as human prose spread across
//! `CHANGELOG.md`, `ARCHITECTURE.md` (ADRs), and `COMPETITIVE.md`. This module
//! makes that history a **machine-readable, self-verifying asset**: a structured
//! causal record of every change (what / why / measured-or-verified effect /
//! lifecycle status), parsed with the crate's own zero-dependency JSON reader.
//!
//! Why this exists, in the framing of recursive self-improvement: the durable
//! asset of a long-lived system is not the model but its *verified improvement
//! history* (Compression in `RSI = Search x Verification x Compression`). The
//! ledger is the Compression layer; the compile-time validator (`tests`) is the
//! Verifier applied to the asset itself. Everything is std-only and offline; no
//! PII is recorded (I3) — entries describe engineering changes, never user data.

use crate::json::{parse, JsonValue};

/// Lifecycle status of an improvement (Improvement Lifecycle Management, #148).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Implemented and verified (tests/eval green).
    Shipped,
    /// Designed/backlogged but deliberately not yet built.
    Deferred,
    /// Once shipped, later removed or superseded (Improvement Half-Life, #164).
    Retired,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Shipped => "shipped",
            Status::Deferred => "deferred",
            Status::Retired => "retired",
        }
    }

    pub fn from_str(s: &str) -> Option<Status> {
        match s {
            "shipped" => Some(Status::Shipped),
            "deferred" => Some(Status::Deferred),
            "retired" => Some(Status::Retired),
            _ => None,
        }
    }
}

/// Risk tier of a change (the axis a human reviewer's attention should track).
/// Stored explicitly via the optional `"risk"` field, or inferred from the
/// change text when absent. Drives the auto-approval gate (IMP-approval-gate):
/// a `High` change always needs human review regardless of test coverage,
/// because tests do not catch every privacy/security regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Pure additive API-surface parity; reversible; no behaviour change to
    /// existing requests. The safe majority that machine checks can auto-approve.
    Low,
    /// Changes routing/cache/cloud/backend behaviour (could alter outputs or cost).
    Medium,
    /// Touches a security/privacy/auth invariant, or removes/retires capability.
    /// Never auto-approved — this is where scarce human review is spent.
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }

    pub fn from_str(s: &str) -> Option<Risk> {
        match s {
            "low" => Some(Risk::Low),
            "medium" => Some(Risk::Medium),
            "high" => Some(Risk::High),
            _ => None,
        }
    }
}

/// The outcome of the machine approval gate for one entry. `Auto` means every
/// machine-checkable invariant held, so no human review is required; this is the
/// lever that keeps human-approval cost sub-linear in ledger size (recursive
/// self-improvement: the Verifier substitutes for the reviewer on the safe set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// All invariants satisfied: complete record, cited grounding, verification
    /// evidence in `effect`, and not high-risk. No human review needed.
    Auto,
    /// One or more invariants failed; the listed reasons tell a human exactly
    /// what to look at, instead of re-reading the whole entry.
    NeedsReview(Vec<&'static str>),
}

impl Approval {
    pub fn is_auto(&self) -> bool {
        matches!(self, Approval::Auto)
    }
}

/// One causal improvement record (Improvement Explainability, #136): the change,
/// why it was made, and the effect that was measured or verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Improvement {
    pub id: String,
    pub title: String,
    pub change: String,
    pub reason: String,
    pub effect: String,
    pub status: Status,
    /// ADR / arXiv / peer-tool anchor. May be empty.
    pub grounding: String,
    /// Explicit risk tier (optional). `None` -> inferred from the change text.
    pub risk: Option<Risk>,
}

impl Improvement {
    /// An entry is valid when every causal field is present and the status is
    /// one of the known lifecycle states. This is the unit the Verifier checks:
    /// a record with no reason or no effect is not an explainable improvement.
    pub fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && !self.title.is_empty()
            && !self.change.is_empty()
            && !self.reason.is_empty()
            && !self.effect.is_empty()
    }

    /// Risk tier: the stored `risk` field if present, else inferred from the
    /// change/reason/title text. Inference is deliberately over-cautious — a
    /// false "High" only costs an unnecessary review, a false "Low" could let a
    /// privacy regression through (same asymmetry as the privacy classifier, I3).
    pub fn inferred_risk(&self) -> Risk {
        if let Some(r) = self.risk {
            return r;
        }
        // A retired entry removed or superseded shipped capability — always a
        // review-worthy event, independent of any text match.
        if self.status == Status::Retired {
            return Risk::High;
        }
        let hay = format!(
            "{} {} {}",
            self.title.to_lowercase(),
            self.change.to_lowercase(),
            self.reason.to_lowercase()
        );
        // HIGH words are deliberately security/privacy-specific. Generic verbs
        // ("remove", "delete") and the overloaded "token" (token *counts* are a
        // core LLM unit, not a secret) are excluded to avoid false High, which
        // would bloat the human review queue and defeat the gate's purpose.
        const HIGH: &[&str] = &[
            "privacy",
            "pii",
            "sensitive",
            "auth",
            "security",
            "secret",
            "credential",
            "password",
            "byok",
            "api key",
            "leak",
            "breaking change",
        ];
        const MEDIUM: &[&str] = &[
            "routing", "route", "cache", "cloud", "escalat", "threshold", "backend", "model",
        ];
        if HIGH.iter().any(|k| hay.contains(k)) {
            Risk::High
        } else if MEDIUM.iter().any(|k| hay.contains(k)) {
            Risk::Medium
        } else {
            Risk::Low
        }
    }

    /// Does the `effect` field carry evidence the change was actually verified
    /// (a test count, "verified", "tested", "eval")? Presence of an effect is
    /// not the same as evidence it was checked; the gate demands the latter.
    pub fn has_verification_evidence(&self) -> bool {
        let e = self.effect.to_lowercase();
        e.contains("test") || e.contains("verified") || e.contains("eval") || e.contains("proven")
    }

    /// Run the machine approval gate. Auto-approvable iff the record is complete,
    /// grounding (provenance) is cited, `effect` shows verification evidence, and
    /// the change is not high-risk. Otherwise return the specific failed checks so
    /// a human reviews *why*, not the whole entry. This is the human-approval-cost
    /// lever: the safe, verified, cited, low-risk majority needs no human at all.
    pub fn approval(&self) -> Approval {
        let mut reasons: Vec<&'static str> = Vec::new();
        if !self.is_valid() {
            reasons.push("incomplete causal record");
        }
        if self.grounding.trim().is_empty() {
            reasons.push("no grounding / provenance cited");
        }
        if !self.has_verification_evidence() {
            reasons.push("effect cites no test / verification evidence");
        }
        if self.inferred_risk() == Risk::High {
            reasons.push("high-risk surface (security / privacy / removal)");
        }
        if reasons.is_empty() {
            Approval::Auto
        } else {
            Approval::NeedsReview(reasons)
        }
    }
}

/// Split a ledger into (auto-approved, needs-review) while preserving order.
/// The first set is what the machine signs off without a human; the second is
/// the (ideally small) queue a reviewer actually has to read.
pub fn partition_for_review(items: &[Improvement]) -> (Vec<&Improvement>, Vec<&Improvement>) {
    let mut auto = Vec::new();
    let mut review = Vec::new();
    for it in items {
        if it.approval().is_auto() {
            auto.push(it);
        } else {
            review.push(it);
        }
    }
    (auto, review)
}

/// Parse a single JSONL line into an `Improvement`. Returns `None` for blank
/// lines, comments (`#`), malformed JSON, or an unknown/missing `status`.
pub fn parse_line(line: &str) -> Option<Improvement> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let v = parse(line).ok()?;
    let s = |key: &str| {
        v.get(key)
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string()
    };
    let status = Status::from_str(v.get("status").and_then(JsonValue::as_str)?)?;
    Some(Improvement {
        id: s("id"),
        title: s("title"),
        change: s("change"),
        reason: s("reason"),
        effect: s("effect"),
        status,
        grounding: s("grounding"),
        risk: v.get("risk").and_then(JsonValue::as_str).and_then(Risk::from_str),
    })
}

/// Parse a whole ledger body (one JSON object per line).
pub fn parse_ledger(content: &str) -> Vec<Improvement> {
    content.lines().filter_map(parse_line).collect()
}

/// Read and parse a ledger file. A missing file yields an empty ledger (so the
/// command degrades gracefully when run outside the repo), matching `cost::read_log`.
pub fn read_ledger(path: &str) -> std::io::Result<Vec<Improvement>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(parse_ledger(&content))
}

/// Aggregate counts over the ledger by lifecycle status and approval gate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LedgerSummary {
    pub total: usize,
    pub shipped: usize,
    pub deferred: usize,
    pub retired: usize,
    /// Entries the machine gate auto-approves (no human review needed).
    pub auto_approved: usize,
    /// Entries that need a human's eyes (the actual review queue).
    pub needs_review: usize,
}

impl LedgerSummary {
    /// Fraction of entries the machine signed off without a human (0.0..=1.0).
    /// This is the human-approval-cost reduction, made measurable.
    pub fn auto_approval_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.auto_approved as f64 / self.total as f64
        }
    }
}

/// Summarize a ledger by status and by the machine approval gate.
pub fn summarize(items: &[Improvement]) -> LedgerSummary {
    let mut s = LedgerSummary::default();
    for it in items {
        s.total += 1;
        match it.status {
            Status::Shipped => s.shipped += 1,
            Status::Deferred => s.deferred += 1,
            Status::Retired => s.retired += 1,
        }
        if it.approval().is_auto() {
            s.auto_approved += 1;
        } else {
            s.needs_review += 1;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_line() {
        let line = r#"{"id":"IMP-X","title":"T","change":"C","reason":"R","effect":"E","status":"shipped"}"#;
        let imp = parse_line(line).expect("should parse");
        assert_eq!(imp.id, "IMP-X");
        assert_eq!(imp.status, Status::Shipped);
        assert!(imp.is_valid());
        assert_eq!(imp.grounding, ""); // optional, absent -> empty
    }

    #[test]
    fn test_parse_skips_blank_and_comment() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("# a comment").is_none());
    }

    #[test]
    fn test_parse_rejects_unknown_status() {
        let line =
            r#"{"id":"IMP-X","title":"T","change":"C","reason":"R","effect":"E","status":"bogus"}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn test_parse_rejects_missing_status() {
        let line = r#"{"id":"IMP-X","title":"T","change":"C","reason":"R","effect":"E"}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn test_is_valid_requires_causal_fields() {
        // Missing reason -> not an explainable improvement.
        let line = r#"{"id":"IMP-X","title":"T","change":"C","reason":"","effect":"E","status":"shipped"}"#;
        let imp = parse_line(line).unwrap();
        assert!(!imp.is_valid());
    }

    #[test]
    fn test_summarize_counts_by_status() {
        let body = concat!(
            r#"{"id":"a","title":"t","change":"c","reason":"r","effect":"e","status":"shipped"}"#,
            "\n",
            r#"{"id":"b","title":"t","change":"c","reason":"r","effect":"e","status":"deferred"}"#,
            "\n",
            r#"{"id":"c","title":"t","change":"c","reason":"r","effect":"e","status":"shipped"}"#,
        );
        let items = parse_ledger(body);
        let s = summarize(&items);
        assert_eq!(s.total, 3);
        assert_eq!(s.shipped, 2);
        assert_eq!(s.deferred, 1);
        assert_eq!(s.retired, 0);
    }

    #[test]
    fn test_read_missing_file_is_empty() {
        let items = read_ledger("/no/such/ledger.jsonl").unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_explicit_risk_field_parsed() {
        let line = r#"{"id":"X","title":"T","change":"C","reason":"R","effect":"E","status":"shipped","risk":"high"}"#;
        let imp = parse_line(line).unwrap();
        assert_eq!(imp.risk, Some(Risk::High));
        assert_eq!(imp.inferred_risk(), Risk::High);
    }

    #[test]
    fn test_inferred_risk_high_on_privacy_surface() {
        let line = r#"{"id":"X","title":"PII detection hardening","change":"add detectors","reason":"prevent leak of sensitive data","effect":"26 tests","status":"shipped"}"#;
        let imp = parse_line(line).unwrap();
        assert_eq!(imp.inferred_risk(), Risk::High);
    }

    #[test]
    fn test_inferred_risk_medium_on_routing_change() {
        let line = r#"{"id":"X","title":"routing tweak","change":"adjust escalation threshold","reason":"better cost","effect":"3 tests","status":"shipped"}"#;
        let imp = parse_line(line).unwrap();
        assert_eq!(imp.inferred_risk(), Risk::Medium);
    }

    #[test]
    fn test_inferred_risk_low_on_additive_surface() {
        let line = r#"{"id":"X","title":"add health field","change":"include version in /health body","reason":"clients detect version mismatch","effect":"4 tests","status":"shipped","grounding":"ADR-X"}"#;
        let imp = parse_line(line).unwrap();
        assert_eq!(imp.inferred_risk(), Risk::Low);
    }

    #[test]
    fn test_approval_auto_for_low_risk_cited_tested() {
        let line = r#"{"id":"X","title":"add health field","change":"include version in /health body","reason":"clients detect version mismatch","effect":"4 tests","status":"shipped","grounding":"ADR-X"}"#;
        let imp = parse_line(line).unwrap();
        assert_eq!(imp.approval(), Approval::Auto);
    }

    #[test]
    fn test_approval_needs_review_for_high_risk_even_if_tested() {
        // High-risk (privacy) entries always need a human, even with tests + grounding.
        let line = r#"{"id":"X","title":"PII hardening","change":"new sensitive detectors","reason":"avoid leak","effect":"26 tests","status":"shipped","grounding":"ADR-X"}"#;
        let imp = parse_line(line).unwrap();
        match imp.approval() {
            Approval::NeedsReview(reasons) => {
                assert!(reasons.iter().any(|r| r.contains("high-risk")));
            }
            Approval::Auto => panic!("high-risk entry must not auto-approve"),
        }
    }

    #[test]
    fn test_approval_needs_review_without_grounding_or_evidence() {
        // Deferred item: no test evidence, no grounding -> needs review.
        let line = r#"{"id":"X","title":"future thing","change":"plan something neutral","reason":"someday","effect":"not yet implemented","status":"deferred"}"#;
        let imp = parse_line(line).unwrap();
        match imp.approval() {
            Approval::NeedsReview(reasons) => {
                assert!(reasons.iter().any(|r| r.contains("verification")));
                assert!(reasons.iter().any(|r| r.contains("grounding")));
            }
            Approval::Auto => panic!("unverified, ungrounded entry must not auto-approve"),
        }
    }

    #[test]
    fn test_partition_for_review_splits_and_preserves_order() {
        let body = concat!(
            r#"{"id":"a","title":"add field","change":"add api field","reason":"client parity","effect":"2 tests","status":"shipped","grounding":"ADR-a"}"#,
            "\n",
            r#"{"id":"b","title":"auth gate","change":"add auth token","reason":"security","effect":"5 tests","status":"shipped","grounding":"ADR-b"}"#,
        );
        let items = parse_ledger(body);
        let (auto, review) = partition_for_review(&items);
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].id, "a");
        assert_eq!(review.len(), 1);
        assert_eq!(review[0].id, "b"); // auth -> high risk
    }

    #[test]
    fn test_summary_reports_approval_counts() {
        let body = concat!(
            r#"{"id":"a","title":"add field","change":"add api field","reason":"client parity","effect":"2 tests","status":"shipped","grounding":"ADR-a"}"#,
            "\n",
            r#"{"id":"b","title":"auth gate","change":"add auth token","reason":"security","effect":"5 tests","status":"shipped","grounding":"ADR-b"}"#,
        );
        let s = summarize(&parse_ledger(body));
        assert_eq!(s.auto_approved, 1);
        assert_eq!(s.needs_review, 1);
        assert!((s.auto_approval_rate() - 0.5).abs() < 1e-9);
    }

    /// The Verifier applied to the asset itself: the bundled ledger MUST parse
    /// cleanly and every entry MUST be a valid, explainable improvement. This is
    /// checked at compile/CI time so the improvement record cannot rot silently
    /// (Continuous Validation, #175).
    #[test]
    fn test_bundled_ledger_is_valid() {
        let body = include_str!("../IMPROVEMENTS.jsonl");
        let items = parse_ledger(body);
        // Every non-blank, non-comment line must have parsed.
        let meaningful = body
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .count();
        assert_eq!(
            items.len(),
            meaningful,
            "some ledger lines failed to parse (bad JSON or unknown status)"
        );
        assert!(!items.is_empty(), "bundled ledger should not be empty");
        for it in &items {
            assert!(
                it.is_valid(),
                "invalid (non-explainable) ledger entry: {}",
                it.id
            );
        }
        // At least the shipped IMP-1 anchor should be present.
        assert!(items
            .iter()
            .any(|i| i.id == "IMP-1" && i.status == Status::Shipped));
    }
}

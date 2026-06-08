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

/// Aggregate counts over the ledger by lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LedgerSummary {
    pub total: usize,
    pub shipped: usize,
    pub deferred: usize,
    pub retired: usize,
}

/// Summarize a ledger by status.
pub fn summarize(items: &[Improvement]) -> LedgerSummary {
    let mut s = LedgerSummary::default();
    for it in items {
        s.total += 1;
        match it.status {
            Status::Shipped => s.shipped += 1,
            Status::Deferred => s.deferred += 1,
            Status::Retired => s.retired += 1,
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

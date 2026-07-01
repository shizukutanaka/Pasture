//! PII category visibility tallies, input- and output-side (IMP-28/IMP-33).
//!
//! Input prompts are classified before routing (`privacy::classify`) so
//! sensitive requests stay local — but until now that classification only
//! produced a one-line stderr notice ("sensitive content detected -> keeping
//! local (N categories)") with no persistent, queryable breakdown of *which*
//! categories triggered it. Nor did anything check what a model *echoes
//! back*: a summary of a document containing an email address, or a tool
//! result relayed through a later turn, can carry PII into the response text
//! without ever being flagged on the way in.
//!
//! This module tallies category labels on both sides, opt-in, read-only. It
//! never redacts or mutates request/response text — the value here is
//! visibility, not enforcement; silently rewriting model output (e.g.
//! replacing legitimate code identifiers) would do more harm than the
//! visibility gain is worth. Detection only, categories only, same I5
//! invariant as the input classifier (never log or store the matched
//! value).

use crate::privacy::classify;
use std::collections::HashMap;
use std::sync::Mutex;

/// Running counts of PII categories, keyed by the same stable labels
/// `privacy::classify` returns (e.g. "email", "api_key"). Used for both the
/// input-side tally (IMP-28, fed from an already-computed `SensitivityReport`)
/// and the output-side tally (IMP-33, fed by scanning response text).
#[derive(Debug, Default)]
pub struct OutputPiiStats {
    counts: Mutex<HashMap<&'static str, u64>>,
}

impl OutputPiiStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one tally per category in `categories`. A no-op for an empty slice.
    /// Used directly by the input-side path, which has already run
    /// `privacy::classify` for routing and would otherwise re-scan the text.
    pub fn tally(&self, categories: &[&'static str]) {
        if categories.is_empty() {
            return;
        }
        if let Ok(mut counts) = self.counts.lock() {
            for cat in categories {
                *counts.entry(cat).or_insert(0) += 1;
            }
        }
    }

    /// Scan `text` for PII categories and add one tally per category found.
    /// A no-op (aside from the classify scan) when no categories match.
    pub fn scan(&self, text: &str) {
        let report = classify(text);
        self.tally(&report.categories);
    }

    /// Snapshot of category -> count, sorted by category name for stable
    /// output (used by `stats`/tests).
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        let counts = match self.counts.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<(&'static str, u64)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }

    /// Total number of category tallies recorded (sum across all categories;
    /// a single response with 2 categories contributes 2, not 1).
    pub fn total(&self) -> u64 {
        self.counts.lock().map(|c| c.values().sum()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_clean_response_no_tally() {
        let stats = OutputPiiStats::new();
        stats.scan("The capital of France is Paris.");
        assert_eq!(stats.total(), 0);
        assert!(stats.snapshot().is_empty());
    }

    #[test]
    fn test_tally_direct_categories_without_reclassifying() {
        // IMP-28: the input-side path already has a SensitivityReport from
        // routing and should tally it directly rather than re-scanning text.
        let stats = OutputPiiStats::new();
        stats.tally(&["email", "api_key"]);
        stats.tally(&["email"]);
        assert_eq!(stats.total(), 3);
        let snap = stats.snapshot();
        assert_eq!(snap, vec![("api_key", 1), ("email", 2)]);
    }

    #[test]
    fn test_tally_empty_slice_is_noop() {
        let stats = OutputPiiStats::new();
        stats.tally(&[]);
        assert_eq!(stats.total(), 0);
    }

    #[test]
    fn test_scan_response_with_email_tallies_category() {
        let stats = OutputPiiStats::new();
        stats.scan("Contact the sender at alice@example.com for details.");
        assert_eq!(stats.total(), 1);
        let snap = stats.snapshot();
        assert_eq!(snap, vec![("email", 1)]);
    }

    #[test]
    fn test_scan_accumulates_across_calls() {
        let stats = OutputPiiStats::new();
        stats.scan("Email: bob@example.com");
        stats.scan("Email: carol@example.com");
        stats.scan("No PII here.");
        assert_eq!(stats.total(), 2);
        let snap = stats.snapshot();
        assert_eq!(snap, vec![("email", 2)]);
    }

    #[test]
    fn test_scan_multiple_categories_in_one_response() {
        let stats = OutputPiiStats::new();
        // A response echoing both an email and an API key shape.
        stats.scan("Reach alice@example.com; key sk-abcdefghijklmnopqrstuvwx");
        let snap = stats.snapshot();
        // Both categories present; exact set depends on classify(), but at
        // least email must be there and total must count every category hit.
        assert!(snap.iter().any(|(cat, _)| *cat == "email"));
        assert_eq!(stats.total(), snap.iter().map(|(_, n)| n).sum::<u64>());
    }

    #[test]
    fn test_never_exposes_matched_value_only_category_label() {
        let stats = OutputPiiStats::new();
        stats.scan("secret alice@example.com leaked");
        // The API surface only returns category labels, never the source text.
        for (cat, _) in stats.snapshot() {
            assert!(!cat.contains('@'));
            assert_ne!(cat, "alice@example.com");
        }
    }
}

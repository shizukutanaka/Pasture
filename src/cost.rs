//! Cost accounting. Each completion appends one structured JSONL line
//! (§9.1). No PII is recorded (I5): only route, model, token counts, cost.

use crate::json::escape_string;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// One cost record. Local routes always cost 0.0 USD.
#[derive(Debug, Clone, PartialEq)]
pub struct CostRecord {
    pub ts_secs: u64,
    pub route: &'static str,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: f64,
    pub logprob: Option<f64>,
}

impl CostRecord {
    /// Build a record, stamping the current time.
    pub fn new(
        route: &'static str,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_usd: f64,
    ) -> Self {
        Self {
            ts_secs: now_secs(),
            route,
            model: model.to_string(),
            prompt_tokens,
            completion_tokens,
            cost_usd,
            logprob: None,
        }
    }

    /// Attach the local-answer mean log-probability (cascade confidence signal),
    /// used by `calibrate --logprob`. It is a single number, never content (I5).
    pub fn with_logprob(mut self, logprob: Option<f64>) -> Self {
        self.logprob = logprob;
        self
    }

    /// Serialize to a single JSONL line (no trailing newline).
    pub fn to_jsonl(&self) -> String {
        let lp = match self.logprob {
            Some(v) => format!(",\"logprob\":{}", format_logprob(v)),
            None => String::new(),
        };
        format!(
            "{{\"ts\":{},\"route\":\"{}\",\"model\":\"{}\",\"prompt_tokens\":{},\"completion_tokens\":{},\"cost_usd\":{}{}}}",
            self.ts_secs,
            escape_string(self.route),
            escape_string(&self.model),
            self.prompt_tokens,
            self.completion_tokens,
            format_cost(self.cost_usd),
            lp,
        )
    }

    /// Append this record to the given log file, creating it if needed.
    pub fn append_to(&self, path: &str) -> std::io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{}", self.to_jsonl())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A record read back from the JSONL log (route is owned, unlike `CostRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedRecord {
    /// Unix timestamp (seconds) parsed from the `ts` field (IMP-26: today filter).
    pub ts_secs: u64,
    pub route: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: f64,
    pub logprob: Option<f64>,
}

/// Parse a single JSONL cost line; returns None for blank/malformed lines.
pub fn parse_log_line(line: &str) -> Option<LoggedRecord> {
    let v = crate::json::parse(line.trim()).ok()?;
    let route = v.get("route")?.as_str()?.to_string();
    Some(LoggedRecord {
        ts_secs: num(&v, "ts").map(|f| f as u64).unwrap_or(0),
        route,
        prompt_tokens: num(&v, "prompt_tokens").unwrap_or(0.0) as u64,
        completion_tokens: num(&v, "completion_tokens").unwrap_or(0.0) as u64,
        cost_usd: num(&v, "cost_usd").unwrap_or(0.0),
        logprob: num(&v, "logprob"),
    })
}

/// Unix timestamp (seconds) for the start of today (00:00:00 UTC).
pub fn today_start_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400 * 86400)
        .unwrap_or(0)
}

/// Sum of cloud prompt+completion tokens logged today (UTC day).
/// Used by IMP-26 to initialise the budget counter from an existing cost log.
pub fn today_cloud_tokens(path: &str) -> u64 {
    let today = today_start_secs();
    let Ok(records) = read_log(path) else {
        return 0;
    };
    records
        .iter()
        .filter(|r| r.ts_secs >= today && r.route == "cloud")
        .map(|r| r.prompt_tokens + r.completion_tokens)
        .sum()
}

fn num(v: &crate::json::JsonValue, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(crate::json::JsonValue::Number(f)) => Some(*f),
        _ => None,
    }
}

/// Read and parse all records from a cost log file (missing file -> empty).
pub fn read_log(path: &str) -> std::io::Result<Vec<LoggedRecord>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(content.lines().filter_map(parse_log_line).collect())
}

/// Aggregate view of a cost log.
#[derive(Debug, Clone, PartialEq)]
pub struct CostSummary {
    pub total: u64,
    pub local: u64,
    pub cloud: u64,
    pub cache: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cloud_cost_usd: f64,
}

impl CostSummary {
    pub fn cloud_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.cloud as f64 / self.total as f64
        }
    }

    pub fn cache_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.cache as f64 / self.total as f64
        }
    }

    /// Machine-readable summary for `pasture stats --json` (CI / dashboards).
    /// `cloud_cost_usd` is guaranteed finite by `summarize`.
    pub fn to_json(&self) -> String {
        let cost = if self.cloud_cost_usd.is_finite() {
            self.cloud_cost_usd
        } else {
            0.0
        };
        format!(
            "{{\"total\":{},\"local\":{},\"cloud\":{},\"cache\":{},\"cloud_rate\":{:.6},\"cache_rate\":{:.6},\"prompt_tokens\":{},\"completion_tokens\":{},\"cloud_cost_usd\":{:.6}}}",
            self.total,
            self.local,
            self.cloud,
            self.cache,
            self.cloud_rate(),
            self.cache_rate(),
            self.prompt_tokens,
            self.completion_tokens,
            cost
        )
    }
}

/// Summarise parsed records by route, tokens, and spend.
/// Distribution of the cascade confidence signal (local mean log-probability).
#[derive(Debug, Clone, PartialEq)]
pub struct LogprobStats {
    pub count: usize,
    pub mean: f64,
    pub min: f64,
    pub p10: f64,
    pub median: f64,
}

/// Summarize the logged local mean log-probabilities, if any are present.
/// Lower values mean the local model was less confident; the low percentiles
/// are what `calibrate --logprob` uses to pick an escalation threshold.
pub fn logprob_summary(records: &[LoggedRecord]) -> Option<LogprobStats> {
    // Skip non-finite values: a corrupt log line (e.g. "logprob":1e400 parses to
    // inf) must not poison the mean/percentiles used by `calibrate --logprob`.
    let mut lps: Vec<f64> = records
        .iter()
        .filter_map(|r| r.logprob)
        .filter(|v| v.is_finite())
        .collect();
    if lps.is_empty() {
        return None;
    }
    lps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = lps.len();
    // n >= 1 guaranteed by the is_empty() guard above; n.saturating_sub(1)
    // makes the invariant self-documenting and safe if the guard is ever moved.
    let quantile = |q: f64| lps[((q * n as f64).floor() as usize).min(n.saturating_sub(1))];
    Some(LogprobStats {
        count: n,
        mean: lps.iter().sum::<f64>() / n as f64,
        min: lps[0],
        p10: quantile(0.10),
        median: quantile(0.50),
    })
}

/// Fold one record into a running summary (route counts, tokens, spend). Shared
/// by `summarize` (whole-slice) and the proxy's incremental metrics cache (IMP-32,
/// ADR-151) so both produce identical aggregates.
pub fn fold_record(s: &mut CostSummary, r: &LoggedRecord) {
    s.total += 1;
    match r.route.as_str() {
        "local" => s.local += 1,
        "cloud" => s.cloud += 1,
        "cache" => s.cache += 1,
        _ => {}
    }
    s.prompt_tokens += r.prompt_tokens;
    s.completion_tokens += r.completion_tokens;
    // Ignore non-finite costs (corrupt log line) so one bad record cannot
    // turn the whole spend total into NaN/inf.
    if r.cost_usd.is_finite() {
        s.cloud_cost_usd += r.cost_usd;
    }
}

pub fn summarize(records: &[LoggedRecord]) -> CostSummary {
    let mut s = CostSummary {
        total: 0,
        local: 0,
        cloud: 0,
        cache: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        cloud_cost_usd: 0.0,
    };
    for r in records {
        fold_record(&mut s, r);
    }
    s
}

/// Format an f64 as a valid JSON number with up to `decimals` places,
/// trimming trailing zeros. Non-finite values (inf/NaN) serialise as `0` —
/// they are invalid JSON numbers and would corrupt the log file. A negative
/// zero that trims to a bare `-` also collapses to `0`.
fn format_number(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let s = format!("{v:.decimals$}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Format a USD cost with up to 6 decimal places.
fn format_cost(cost: f64) -> String {
    format_number(cost, 6)
}

/// Format a (typically negative) mean log-probability with up to 4 decimals.
fn format_logprob(v: f64) -> String {
    format_number(v, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_jsonl_local_zero_cost() {
        let r = CostRecord {
            ts_secs: 100,
            route: "local",
            model: "llama3".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_usd: 0.0,
            logprob: None,
        };
        let line = r.to_jsonl();
        assert!(line.contains("\"route\":\"local\""));
        assert!(line.contains("\"cost_usd\":0"));
        assert!(line.contains("\"prompt_tokens\":10"));
    }

    #[test]
    fn test_to_jsonl_cloud_cost_formatted() {
        let r = CostRecord {
            ts_secs: 1,
            route: "cloud",
            model: "gpt".to_string(),
            prompt_tokens: 1000,
            completion_tokens: 200,
            cost_usd: 0.0125,
            logprob: None,
        };
        assert!(r.to_jsonl().contains("\"cost_usd\":0.0125"));
    }

    #[test]
    fn test_format_cost_trims_zeros() {
        assert_eq!(format_cost(0.0), "0");
        assert_eq!(format_cost(1.5), "1.5");
        assert_eq!(format_cost(0.010000), "0.01");
    }

    #[test]
    fn test_to_jsonl_is_valid_json() {
        let r = CostRecord::new("cloud", "m\"odel", 1, 1, 0.001);
        let parsed = crate::json::parse(&r.to_jsonl());
        assert!(parsed.is_ok(), "JSONL line should parse: {}", r.to_jsonl());
    }

    #[test]
    fn test_append_to_writes_line() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pasture-cost-test-{}.jsonl", now_secs()));
        let p = path.to_str().unwrap();
        let r = CostRecord::new("local", "llama3", 3, 2, 0.0);
        r.append_to(p).unwrap();
        let content = std::fs::read_to_string(p).unwrap();
        assert!(content.trim().ends_with('}'));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn test_parse_log_line_roundtrip() {
        let line = CostRecord::new("cloud", "gpt", 10, 4, 0.002).to_jsonl();
        let parsed = parse_log_line(&line).unwrap();
        assert_eq!(parsed.route, "cloud");
        assert_eq!(parsed.prompt_tokens, 10);
        assert_eq!(parsed.completion_tokens, 4);
        assert!((parsed.cost_usd - 0.002).abs() < 1e-9);
    }

    #[test]
    fn test_parse_log_line_rejects_garbage() {
        assert!(parse_log_line("not json").is_none());
        assert!(parse_log_line("").is_none());
    }

    #[test]
    fn test_summarize_counts_and_rates() {
        let recs = vec![
            LoggedRecord {
                ts_secs: 0,
                route: "local".into(),
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_usd: 0.0,
                logprob: None,
            },
            LoggedRecord {
                ts_secs: 0,
                route: "cloud".into(),
                prompt_tokens: 20,
                completion_tokens: 8,
                cost_usd: 0.01,
                logprob: None,
            },
            LoggedRecord {
                ts_secs: 0,
                route: "cache".into(),
                prompt_tokens: 0,
                completion_tokens: 0,
                cost_usd: 0.0,
                logprob: None,
            },
            LoggedRecord {
                ts_secs: 0,
                route: "cloud".into(),
                prompt_tokens: 30,
                completion_tokens: 2,
                cost_usd: 0.02,
                logprob: None,
            },
        ];
        let s = summarize(&recs);
        assert_eq!((s.total, s.local, s.cloud, s.cache), (4, 1, 2, 1));
        assert_eq!(s.prompt_tokens, 60);
        assert_eq!(s.completion_tokens, 15);
        assert!((s.cloud_cost_usd - 0.03).abs() < 1e-9);
        assert!((s.cloud_rate() - 0.5).abs() < 1e-9);
        assert!((s.cache_rate() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn test_summarize_empty() {
        let s = summarize(&[]);
        assert_eq!(s.total, 0);
        assert_eq!(s.cloud_rate(), 0.0);
    }

    #[test]
    fn test_logprob_summary() {
        let mk = |lp: Option<f64>| LoggedRecord {
            ts_secs: 0,
            route: "local".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            cost_usd: 0.0,
            logprob: lp,
        };
        assert!(logprob_summary(&[mk(None), mk(None)]).is_none());
        let recs: Vec<LoggedRecord> = (1..=10).map(|i| mk(Some(-(i as f64) / 10.0))).collect();
        let st = logprob_summary(&recs).unwrap();
        assert_eq!(st.count, 10);
        assert!((st.mean - (-0.55)).abs() < 1e-9, "{}", st.mean);
        assert!((st.min - (-1.0)).abs() < 1e-9);
        assert!(st.p10 <= st.median);
    }

    #[test]
    fn test_summary_to_json_roundtrips() {
        let recs = vec![
            LoggedRecord {
                ts_secs: 0,
                route: "cloud".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                cost_usd: 0.0125,
                logprob: None,
            },
            LoggedRecord {
                ts_secs: 0,
                route: "local".into(),
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_usd: 0.0,
                logprob: None,
            },
        ];
        let json = summarize(&recs).to_json();
        let v = crate::json::parse(&json).expect("stats --json must be valid JSON");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(2.0));
        assert_eq!(v.get("cloud").and_then(|x| x.as_f64()), Some(1.0));
        assert_eq!(v.get("prompt_tokens").and_then(|x| x.as_f64()), Some(110.0));
        assert!((v.get("cloud_rate").and_then(|x| x.as_f64()).unwrap() - 0.5).abs() < 1e-9);
        assert!((v.get("cloud_cost_usd").and_then(|x| x.as_f64()).unwrap() - 0.0125).abs() < 1e-6);
    }

    #[test]
    fn test_summary_to_json_empty_is_valid() {
        let json = summarize(&[]).to_json();
        let v = crate::json::parse(&json).expect("empty stats --json must be valid JSON");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(0.0));
        assert_eq!(v.get("cloud_rate").and_then(|x| x.as_f64()), Some(0.0));
    }

    #[test]
    fn test_format_cost_non_finite_is_zero() {
        assert_eq!(format_cost(f64::INFINITY), "0");
        assert_eq!(format_cost(f64::NEG_INFINITY), "0");
        assert_eq!(format_cost(f64::NAN), "0");
    }

    #[test]
    fn test_format_logprob_non_finite_is_zero() {
        assert_eq!(format_logprob(f64::INFINITY), "0");
        assert_eq!(format_logprob(f64::NEG_INFINITY), "0");
        assert_eq!(format_logprob(f64::NAN), "0");
    }

    #[test]
    fn test_to_jsonl_non_finite_cost_is_valid_json() {
        // A CostRecord with a non-finite cost (e.g. from upstream bug) must
        // still produce a valid JSON line — not "inf" or "NaN".
        let r = CostRecord {
            ts_secs: 1,
            route: "cloud",
            model: "m".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            cost_usd: f64::INFINITY,
            logprob: Some(f64::NAN),
        };
        let line = r.to_jsonl();
        assert!(crate::json::parse(&line).is_ok(), "line must be valid JSON: {line}");
        assert!(!line.contains("inf"), "inf must not appear in JSONL");
        assert!(!line.contains("NaN"), "NaN must not appear in JSONL");
    }

    #[test]
    fn test_non_finite_records_do_not_poison_aggregates() {
        // A corrupt log line (inf/NaN) must not make the spend total or logprob
        // stats non-finite. inf cost is ignored; inf logprob is filtered out.
        let recs = vec![
            LoggedRecord {
                ts_secs: 0,
                route: "cloud".into(),
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_usd: 0.02,
                logprob: Some(-0.5),
            },
            LoggedRecord {
                ts_secs: 0,
                route: "cloud".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
                cost_usd: f64::INFINITY, // corrupt
                logprob: Some(f64::NAN), // corrupt
            },
        ];
        let s = summarize(&recs);
        assert!(s.cloud_cost_usd.is_finite());
        assert!((s.cloud_cost_usd - 0.02).abs() < 1e-9);
        let st = logprob_summary(&recs).unwrap();
        assert_eq!(st.count, 1); // only the finite logprob counts
        assert!(st.mean.is_finite());
        assert!((st.mean - (-0.5)).abs() < 1e-9);
    }
}

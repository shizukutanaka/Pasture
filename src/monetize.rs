//! Monetization surfaces: a donation link and cloud-provider referral links.
//!
//! Principles:
//! - No user PII is collected or transmitted (I5). Pasture only *surfaces* URLs.
//! - No secrets here. Donation runs through a hosted Stripe link/Worker that
//!   the operator configures; Pasture never sees a card or a Stripe key.
//! - Referral URLs are the operator's own affiliate links, supplied via
//!   configuration. None are fabricated or shipped with codes.
//! - Nudges are gentle and infrequent (Apple-style restraint, I3); never block.

/// A cloud provider that can be referred. `homepage` is informational only;
/// the actual affiliate URL (with the operator's code) comes from config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub key: &'static str,
    pub display: &'static str,
    pub homepage: &'static str,
}

/// Known providers Pasture can route to / refer. Affiliate codes are NOT
/// included; configure them via `PASTURE_REF_<KEY>` (see `referral_url`).
pub const KNOWN_PROVIDERS: &[Provider] = &[
    Provider {
        key: "openrouter",
        display: "OpenRouter",
        homepage: "https://openrouter.ai",
    },
    Provider {
        key: "runpod",
        display: "RunPod (GPU rental)",
        homepage: "https://www.runpod.io",
    },
    Provider {
        key: "vastai",
        display: "Vast.ai (GPU rental)",
        homepage: "https://vast.ai",
    },
    Provider {
        key: "together",
        display: "Together AI",
        homepage: "https://www.together.ai",
    },
];

/// Look up a known provider by key.
pub fn provider(key: &str) -> Option<&'static Provider> {
    KNOWN_PROVIDERS.iter().find(|p| p.key == key)
}

/// Resolve the configured referral URL for a provider key, using an injected
/// resolver (e.g. one that reads `PASTURE_REF_<KEY>`). Testable without env.
pub fn referral_url<F>(key: &str, resolver: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let url = resolver(key)?;
    let trimmed = url.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Decide whether to show a donation nudge on this run.
/// Shows once every `every` runs (and never on run 0 or when `every` is 0).
pub fn should_nudge(run_count: u64, every: u64) -> bool {
    every > 0 && run_count > 0 && run_count % every == 0
}

/// The standard donation interval (runs between nudges).
pub const NUDGE_EVERY: u64 = 25;

/// Message for the `donate` command.
pub fn donation_text(url: Option<&str>) -> String {
    match url {
        Some(u) => format!(
            "Pasture is free and ad-free. If it saves you money, $1/month keeps it going:\n  {u}\nThank you."
        ),
        None => "Donation link is not configured.\nSet PASTURE_DONATE_URL to your hosted Stripe link, then run `pasture donate` again.".to_string(),
    }
}

/// One-line gentle nudge appended after a command (only when due).
pub fn nudge_text(url: Option<&str>) -> Option<String> {
    url.map(|u| format!("— enjoying Pasture? support $1/mo: {u} (set PASTURE_NO_NUDGE=1 to hide)"))
}

/// Build the `refer` command output for all known providers.
pub fn referral_list_text<F>(resolver: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut out =
        String::from("Cloud providers (configure your affiliate URL via PASTURE_REF_<KEY>):\n");
    for p in KNOWN_PROVIDERS {
        match referral_url(p.key, &resolver) {
            Some(u) => out.push_str(&format!(
                "  {:<12} {}  [configured] {}\n",
                p.key, p.display, u
            )),
            None => out.push_str(&format!("  {:<12} {}  {}\n", p.key, p.display, p.homepage)),
        }
    }
    out.push_str("Referral clicks carry no user identifiers (I5).");
    out
}

// --- run-count state (for nudge cadence) ---

/// Parse a stored run count; any malformed content resets to 0.
pub fn parse_count(contents: &str) -> u64 {
    contents.trim().parse().unwrap_or(0)
}

/// Read the run count from a state file (0 if absent/unreadable).
pub fn read_count(path: &str) -> u64 {
    std::fs::read_to_string(path)
        .map(|c| parse_count(&c))
        .unwrap_or(0)
}

/// Increment and persist the run count, returning the new value. A write
/// failure is non-fatal: it just means the nudge cadence may not advance.
pub fn bump_count(path: &str) -> u64 {
    let next = read_count(path).saturating_add(1);
    let _ = std::fs::write(path, next.to_string());
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_provider_lookup() {
        assert_eq!(provider("openrouter").unwrap().display, "OpenRouter");
        assert!(provider("nonexistent").is_none());
    }

    #[test]
    fn test_referral_url_configured() {
        let map: HashMap<&str, String> =
            [("openrouter", "https://openrouter.ai/?ref=abc".to_string())].into();
        let r = referral_url("openrouter", |k| map.get(k).cloned());
        assert_eq!(r.as_deref(), Some("https://openrouter.ai/?ref=abc"));
    }

    #[test]
    fn test_referral_url_unconfigured_is_none() {
        let r = referral_url("openrouter", |_| None);
        assert!(r.is_none());
    }

    #[test]
    fn test_referral_url_blank_is_none() {
        let r = referral_url("openrouter", |_| Some("   ".to_string()));
        assert!(r.is_none());
    }

    #[test]
    fn test_should_nudge_cadence() {
        assert!(!should_nudge(0, 25));
        assert!(!should_nudge(1, 25));
        assert!(should_nudge(25, 25));
        assert!(should_nudge(50, 25));
        assert!(!should_nudge(26, 25));
    }

    #[test]
    fn test_should_nudge_zero_interval_never() {
        assert!(!should_nudge(100, 0));
    }

    #[test]
    fn test_donation_text_configured() {
        let t = donation_text(Some("https://pay.example/x"));
        assert!(t.contains("https://pay.example/x"));
        assert!(t.contains("$1/month"));
    }

    #[test]
    fn test_donation_text_unconfigured_explains() {
        let t = donation_text(None);
        assert!(t.contains("PASTURE_DONATE_URL"));
    }

    #[test]
    fn test_nudge_text_none_when_no_url() {
        assert!(nudge_text(None).is_none());
    }

    #[test]
    fn test_referral_list_marks_configured() {
        let map: HashMap<&str, String> =
            [("runpod", "https://runpod.io/?ref=z".to_string())].into();
        let out = referral_list_text(|k| map.get(k).cloned());
        assert!(out.contains("[configured]"));
        assert!(out.contains("openrouter")); // unconfigured still listed
        assert!(out.contains("no user identifiers"));
    }

    #[test]
    fn test_parse_count_valid_and_invalid() {
        assert_eq!(parse_count("42\n"), 42);
        assert_eq!(parse_count("garbage"), 0);
        assert_eq!(parse_count(""), 0);
    }

    #[test]
    fn test_bump_count_persists() {
        let path = std::env::temp_dir()
            .join(format!(
                "pasture-count-{}.txt",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .to_string_lossy()
            .into_owned();
        assert_eq!(bump_count(&path), 1);
        assert_eq!(bump_count(&path), 2);
        assert_eq!(read_count(&path), 2);
        let _ = std::fs::remove_file(&path);
    }
}

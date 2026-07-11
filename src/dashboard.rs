//! The embedded web dashboard (IMP-36/ADR-235).
//!
//! A single self-contained HTML page (inline CSS + vanilla JS, zero external
//! references) baked into the binary at compile time. It is served verbatim at
//! `GET /dashboard` (and `GET /`) and is a pure presentation layer over the
//! existing `GET /v1/stats` JSON endpoint — it polls that endpoint client-side
//! and renders it. No backend data plumbing lives here.
//!
//! This mirrors `improve.rs`'s `include_str!("../IMPROVEMENTS.jsonl")` pattern:
//! keeping the markup in a real `.html` file (rather than an inline string
//! literal) lets it be opened directly in a browser during development and gets
//! proper editor support, while `include_str!` preserves the zero-dependency,
//! single-binary invariant (I1) — no runtime file read, no asset directory.

/// The complete dashboard page, embedded at compile time.
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[cfg(test)]
mod tests {
    use super::DASHBOARD_HTML;

    #[test]
    fn dashboard_is_non_empty_html() {
        assert!(!DASHBOARD_HTML.is_empty());
        assert!(DASHBOARD_HTML.contains("<html"));
        assert!(DASHBOARD_HTML.contains("</html>"));
    }

    #[test]
    fn dashboard_polls_the_stats_endpoint() {
        // The page's entire reason to exist is rendering /v1/stats; if this
        // fetch target were renamed the dashboard would silently show nothing.
        assert!(DASHBOARD_HTML.contains("/v1/stats"));
        assert!(DASHBOARD_HTML.contains("fetch("));
    }

    #[test]
    fn dashboard_has_no_external_references() {
        // Invariant I1: the page must be fully self-contained. Any external
        // <script src>/<link href> would mean a network dependency on a CDN or
        // third-party origin — exactly what Pasture refuses to have. This is a
        // machine check so the guarantee cannot silently erode.
        let lower = DASHBOARD_HTML.to_lowercase();
        assert!(
            !lower.contains("<script src="),
            "dashboard must not load external scripts"
        );
        assert!(
            !lower.contains("<link "),
            "dashboard must not load external stylesheets"
        );
        assert!(
            !lower.contains("http://") && !lower.contains("https://"),
            "dashboard must not reference any external URL"
        );
    }
}

//! Command-line interface. Argument parsing is hand-rolled to keep the binary
//! dependency-free. Pure helpers are unit-tested; commands that touch the
//! network (`chat`, `serve`) are validated manually / via integration.

use crate::backend::{Backend, CompletionRequest, Message, OllamaBackend, OpenAiCompatBackend};
use crate::config::Config;
use crate::hardware::HardwareProfile;
use crate::proxy::Proxy;
use crate::routing::{Route, RoutingEngine};

const USAGE: &str = "\
pasture — OpenAI-compatible local/cloud LLM routing proxy

USAGE:
    pasture <command> [options]

COMMANDS:
    doctor                   Check your setup and show how to fix problems
    setup                    Beginner guide + environment check
    up                       One command: ensure model, then start the proxy
    connect [app]            Show how to point your app at pasture (openwebui/continue/cursor/lmstudio/sdk)
    models                   Recommend local models for your machine
    hw                       Show the detected hardware profile
    route <text> [--json]    Dry-run: show where a prompt would be routed (--json for scripting)
    chat  <text>             Send one prompt through the router (needs Ollama)
    serve                    Start the OpenAI-compatible proxy server
    eval [--external <file>] Measure routing quality + token-threshold sweep (--json for machine output)
    stats [--json]           Summarize the cost log (routes, tokens, spend)
    improvements [path]      Show the self-improvement ledger (verified change history)
    improvements --review    Show only entries the machine gate cannot auto-approve
    config                   Print the effective configuration (no secrets)
    calibrate [--target R]   Recommend PASTURE_THRESHOLD from your logged usage (R=cloud rate, default 0.2)
    calibrate --sweep         Show recommended thresholds at several target rates at once (add --logprob for cascade)
    calibrate --logprob       Recommend PASTURE_CASCADE_LOGPROB from logged cascade confidence
    calibrate --error --labels <f.jsonl> [--target E]
    calibrate --auroc --labels <f.jsonl>     Self-test: does the confidence signal actually predict correctness?
    label --prompts <f> [--out <f>]          Build that labels file: answer prompts locally and mark each right/wrong
                             Recommend PASTURE_CASCADE_LOGPROB for a target error rate E
                             (default 0.1) from labelled answers: {\"logprob\": -0.4, \"correct\": true}
    version                  Print the version
    help                     Show this help

OPTIONS:
    --local                  Force local routing
    --cloud                  Force cloud routing
    --addr <host:port>       Listen address for `serve`
";

/// Parse a forced-route flag from the argument list.
pub fn parse_route_flag(args: &[String]) -> Option<Route> {
    if args.iter().any(|a| a == "--cloud") {
        Some(Route::Cloud)
    } else if args.iter().any(|a| a == "--local") {
        Some(Route::Local)
    } else {
        None
    }
}

/// Collect the positional (non-flag) arguments after the command.
fn positional(args: &[String]) -> Vec<&str> {
    args.iter()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .collect()
}

/// Read the value following a `--flag` option.
fn option_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).map(|s| s.as_str())
}

/// Format a human-readable description of a routing decision (testable).
pub fn route_decision_text(engine: &RoutingEngine, text: &str, forced: Option<Route>) -> String {
    match engine.decide(text, forced) {
        Ok(d) => format!(
            "route: {}\nreason: {}\nthreshold: {} tokens",
            d.route.as_str(),
            d.reason,
            engine.threshold()
        ),
        Err(e) => format!("error: {e}"),
    }
}

/// What the budget/spike guard does to a cloud route (ADR-202).
#[derive(Clone, Copy, PartialEq)]
enum BudgetAction {
    Block,    // reject with 429
    Warn,     // proceed on cloud, over budget
    Redirect, // downgrade to local
}

/// The guard's read-only verdict when it overrides a cloud route.
struct BudgetHit {
    spike: bool, // true = spike guard fired; false = daily budget exhausted
    action: BudgetAction,
    avg: u64, // running cloud-token average (spike denominator)
}

/// A computed routing preview — the single source of truth shared by the human
/// (`route_preview_text`) and machine (`route_preview_json`) renderers, matching
/// the fields HTTP `POST /v1/route` returns (ADR-201/202). `route` is the
/// **effective** route *after* the budget/spike guard, so all three surfaces
/// (CLI text, CLI JSON, HTTP JSON) agree with what would really happen.
struct RoutePreview {
    route: Route,
    reason: String,
    threshold: usize,
    sensitive: bool,
    categories: Vec<&'static str>,
    est_input: u64,
    est_output: u64,
    est_total: u64,
    cost_usd: f64,
    serves_cloud: bool,
    budget_active: bool, // a daily token budget is configured
    today_tokens: u64,
    budget_limit: u64,
    spike_factor: u64,
    hit: Option<BudgetHit>, // Some only when the guard overrides the cloud route
}

/// Compute a routing preview, applying the read-only budget/spike guard so the
/// reported route and cost match reality (ADR-202). Budget/spike state is
/// derived from the cost log (best-effort; the live proxy uses in-memory atomics
/// seeded from the same log at startup). Returns `Err(reason)` if the engine
/// cannot route (e.g. sensitive content with no local backend).
fn compute_route_preview(
    engine: &RoutingEngine,
    config: &Config,
    text: &str,
    forced: Option<Route>,
    today_tokens: u64,
    cloud_count: u64,
    cloud_sum: u64,
) -> Result<RoutePreview, String> {
    let report = crate::privacy::classify(text);
    let decision = engine
        .decide_with_sensitivity(text, forced, report.is_sensitive())
        .map_err(|e| e.to_string())?;

    let est_input = crate::routing::estimate_tokens(text) as u64;
    let est_total = crate::routing::estimate_total_tokens(text, None) as u64;
    let est_output = est_total.saturating_sub(est_input);

    // Read-only budget/spike guard, mirroring Proxy::budget_spike_preview's
    // conditions and apply_budget_guard's action branching (ADR-200). It only
    // applies to an engine "cloud" decision when a guard is configured.
    let guard_configured = config.budget_daily_tokens > 0 || config.spike_factor > 0;
    let (route, serves_cloud, hit) = if decision.route == Route::Cloud && guard_configured {
        let avg = if cloud_count > 0 {
            cloud_sum / cloud_count
        } else {
            0
        };
        let spike_hit = config.spike_factor > 0
            && est_total > 0
            && avg > 0
            && est_total > config.spike_factor.saturating_mul(avg);
        let budget_hit =
            config.budget_daily_tokens > 0 && today_tokens >= config.budget_daily_tokens;
        if spike_hit || budget_hit {
            let action = match config.budget_action.as_str() {
                "block" => BudgetAction::Block,
                "warn" => BudgetAction::Warn,
                _ => BudgetAction::Redirect,
            };
            let hit = BudgetHit {
                spike: spike_hit,
                action,
                avg,
            };
            match action {
                // block/warn keep the cloud route; block won't actually serve it
                // (429), so it costs nothing. redirect downgrades to local.
                BudgetAction::Block => (Route::Cloud, false, Some(hit)),
                BudgetAction::Warn => (Route::Cloud, true, Some(hit)),
                BudgetAction::Redirect => (Route::Local, false, Some(hit)),
            }
        } else {
            (Route::Cloud, true, None)
        }
    } else {
        (decision.route, decision.route == Route::Cloud, None)
    };

    let (in_price, out_price) = config.cloud_price_per_1m;
    let cost_usd = if serves_cloud {
        (est_input as f64 * in_price + est_output as f64 * out_price) / 1_000_000.0
    } else {
        0.0
    };

    Ok(RoutePreview {
        route,
        reason: decision.reason,
        threshold: engine.threshold(),
        sensitive: report.is_sensitive(),
        categories: report.categories,
        est_input,
        est_output,
        est_total,
        cost_usd,
        serves_cloud,
        budget_active: config.budget_daily_tokens > 0,
        today_tokens,
        budget_limit: config.budget_daily_tokens,
        spike_factor: config.spike_factor,
        hit,
    })
}

/// Rich human-readable routing preview for `pasture route <text>` (ADR-201/202).
///
/// Shows sensitivity, the **effective** route (after the budget/spike guard),
/// reason, threshold, token estimates, cost, and budget status — the same facts
/// HTTP `POST /v1/route` returns, formatted for a terminal.
pub fn route_preview_text(
    engine: &RoutingEngine,
    config: &Config,
    text: &str,
    forced: Option<Route>,
    today_tokens: u64,
    cloud_count: u64,
    cloud_sum: u64,
) -> String {
    let p = match compute_route_preview(
        engine,
        config,
        text,
        forced,
        today_tokens,
        cloud_count,
        cloud_sum,
    ) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };

    let mut out = String::new();
    if p.sensitive {
        out.push_str(&format!("sensitive: yes [{}]\n", p.categories.join(", ")));
    } else {
        out.push_str("sensitive: no\n");
    }
    out.push_str(&format!("route:     {}\n", p.route.as_str()));
    out.push_str(&format!("reason:    {}\n", p.reason));
    out.push_str(&format!("threshold: {} tokens\n", p.threshold));
    out.push_str(&format!(
        "estimated: {} in, ~{} predicted total\n",
        p.est_input, p.est_total
    ));

    let (in_price, out_price) = config.cloud_price_per_1m;
    if p.serves_cloud {
        if in_price > 0.0 || out_price > 0.0 {
            out.push_str(&format!(
                "cost:      ${:.6} (${in_price:.2}/${out_price:.2} per 1M in/out)\n",
                p.cost_usd
            ));
        } else {
            out.push_str("cost:      (no price configured — set PASTURE_CLOUD_PRICE_PER_1M)\n");
        }
    } else if p.route == Route::Local {
        out.push_str("cost:      $0.000000 (local — no charge)\n");
    } else {
        // Cloud route the guard won't actually serve (block) — no charge incurred.
        out.push_str("cost:      $0.000000 (not served — over budget)\n");
    }

    // Budget line: spell out an override, confirm headroom, or note it's off.
    match &p.hit {
        Some(h) => {
            let action = match h.action {
                BudgetAction::Block => "would be blocked (429)",
                BudgetAction::Warn => "proceeds with warning",
                BudgetAction::Redirect => "would redirect to local",
            };
            let reason = if h.spike {
                format!(
                    "spike: {} predicted > {}×avg {}",
                    p.est_total, p.spike_factor, h.avg
                )
            } else {
                format!(
                    "daily budget: {}/{} tokens used",
                    p.today_tokens, p.budget_limit
                )
            };
            out.push_str(&format!("budget:    {action} — {reason}\n"));
        }
        None if p.budget_active && p.serves_cloud => {
            out.push_str(&format!(
                "budget:    within budget ({}/{} tokens used today)\n",
                p.today_tokens, p.budget_limit
            ));
        }
        None if !p.budget_active && p.route == Route::Cloud => {
            out.push_str("budget:    not configured\n");
        }
        None => {}
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Machine-readable routing preview for `pasture route --json` (ADR-202).
///
/// Emits the exact same `pasture.route` JSON object that HTTP `POST /v1/route`
/// returns, so a script gets identical structured output whether it shells out
/// to the CLI or calls the proxy. `has_tools` is always `false` (the CLI routes
/// plain text). The `budget` field is `null` unless the guard overrides the
/// cloud route, matching the HTTP endpoint's wording.
pub fn route_preview_json(
    engine: &RoutingEngine,
    config: &Config,
    text: &str,
    forced: Option<Route>,
    today_tokens: u64,
    cloud_count: u64,
    cloud_sum: u64,
) -> String {
    use crate::json::escape_string;
    let p = match compute_route_preview(
        engine,
        config,
        text,
        forced,
        today_tokens,
        cloud_count,
        cloud_sum,
    ) {
        Ok(p) => p,
        Err(e) => {
            return format!(
                "{{\"object\":\"pasture.error\",\"error\":\"{}\"}}",
                escape_string(&e)
            );
        }
    };
    let categories = p
        .categories
        .iter()
        .map(|c| format!("\"{}\"", escape_string(c)))
        .collect::<Vec<_>>()
        .join(",");
    let budget_field = match &p.hit {
        Some(h) => {
            let reason = if h.spike {
                "spike detected — request exceeds average by spike_factor"
            } else {
                "daily cloud token budget exceeded"
            };
            let note = match h.action {
                BudgetAction::Block => format!("would be blocked (429): {reason}"),
                BudgetAction::Warn => format!("over budget, proceeds (warn): {reason}"),
                BudgetAction::Redirect => format!("redirected to local: {reason}"),
            };
            format!("\"{}\"", escape_string(&note))
        }
        None => "null".to_string(),
    };
    format!(
        "{{\"object\":\"pasture.route\",\"route\":\"{}\",\"reason\":\"{}\",\"budget\":{},\"sensitive\":{},\"categories\":[{}],\"estimated_tokens\":{},\"predicted_output_tokens\":{},\"predicted_total_tokens\":{},\"estimated_cost_usd\":{:.6},\"has_tools\":false}}",
        p.route.as_str(),
        escape_string(&p.reason),
        budget_field,
        p.sensitive,
        categories,
        p.est_input,
        p.est_output,
        p.est_total,
        p.cost_usd,
    )
}

/// Format the hardware profile for display (testable).
pub fn hardware_text(p: &HardwareProfile) -> String {
    let gpu = match &p.gpu {
        Some(g) => match g.vram_mb {
            Some(v) => format!("{} ({v} MB VRAM)", g.vendor),
            None => g.vendor.clone(),
        },
        None => "none".to_string(),
    };
    // ADR-261: "unknown" is a real state, not 0 MB.
    let ram = match p.ram_mb {
        Some(mb) => format!("{mb} MB"),
        None => "unknown (detection unavailable on this OS)".to_string(),
    };
    format!("RAM: {ram}\nCPU threads: {}\nGPU: {gpu}", p.cpu_count)
}

/// Entry point. Returns a process exit code.
pub fn run(args: &[String]) -> i32 {
    let command = match args.get(1) {
        Some(c) => c.as_str(),
        None => {
            print!("{}", crate::i18n::t(crate::i18n::detect(), "welcome"));
            return 0;
        }
    };
    let rest = &args[2..];
    let config = Config::default().with_config_file().with_env();
    // ADR-266: a misspelled or malformed PASTURE_* setting is otherwise silently
    // ignored forever. Warn once, to stderr, so normal stdout output is clean.
    for w in crate::config::env_warnings() {
        eprintln!("pasture: {w}");
    }

    match command {
        "version" => {
            println!("pasture {}", crate::VERSION);
            0
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        "doctor" => run_doctor(&config),
        "setup" => {
            print!("{}", crate::i18n::t(crate::i18n::detect(), "welcome"));
            println!("\n--- check ---\n");
            run_doctor(&config)
        }
        "up" => {
            let addr = option_value(rest, "--addr").unwrap_or(&config.listen_addr);
            run_up(&config, addr)
        }
        "connect" => run_connect(&config, positional(rest).first().copied()),
        "models" => run_models(&config),
        "hw" => {
            let profile = HardwareProfile::detect();
            println!("{}", hardware_text(&profile));
            0
        }
        "route" => run_route(&config, rest),
        "chat" => {
            let pos = positional(rest);
            let Some(text) = pos.first() else {
                eprintln!("usage: pasture chat <text> [--local|--cloud]");
                return 2;
            };
            run_chat(&config, text, parse_route_flag(rest))
        }
        "serve" => {
            let addr = option_value(rest, "--addr").unwrap_or(&config.listen_addr);
            run_serve(&config, addr)
        }
        "eval" => run_eval(rest),
        "stats" => run_stats(&config, rest),
        "calibrate" => run_calibrate(&config, rest),
        "label" => run_label(&config, rest),
        "improvements" => {
            let review = rest.iter().any(|a| a == "--review");
            run_improvements(positional(rest).first().copied(), review)
        }
        "config" => run_config(&config),
        other => {
            eprintln!("unknown command: {other}\n");
            print!("{USAGE}");
            2
        }
    }
}

/// `pasture route <text>`: dry-run the routing decision for a prompt.
///
/// Shows sensitivity, route, reason, threshold, token estimates, cost, and
/// budget status — matching the fields returned by HTTP `POST /v1/route`
/// (ADR-201: CLI/HTTP parity).
fn run_route(config: &Config, rest: &[String]) -> i32 {
    let pos = positional(rest);
    let Some(text) = pos.first() else {
        eprintln!("usage: pasture route <text> [--local|--cloud]");
        return 2;
    };
    // Dry-run assumes both backends present (same as before).
    let profile = HardwareProfile::detect();
    let engine = make_engine(&profile, config, true);

    // Derive budget/spike state from the cost log (best-effort; live proxy uses
    // in-memory atomics seeded from the same source at startup).
    let today = crate::cost::today_start_secs();
    let (today_tokens, cloud_count, cloud_sum) = match crate::cost::read_log(&config.cost_log_path)
    {
        Ok(records) => {
            let today_recs: Vec<_> = records
                .iter()
                .filter(|r| r.ts_secs >= today && r.route == "cloud")
                .collect();
            let today_tokens: u64 = today_recs
                .iter()
                .map(|r| r.prompt_tokens + r.completion_tokens)
                .sum();
            let cloud_count = today_recs.len() as u64;
            let cloud_sum: u64 = today_recs
                .iter()
                .map(|r| r.prompt_tokens + r.completion_tokens)
                .sum();
            (today_tokens, cloud_count, cloud_sum)
        }
        Err(_) => (0, 0, 0),
    };

    let forced = parse_route_flag(rest);
    // `--json` emits the same `pasture.route` object as HTTP POST /v1/route, so a
    // script gets identical output from either surface (ADR-202).
    let out = if rest.iter().any(|a| a == "--json") {
        route_preview_json(
            &engine,
            config,
            text,
            forced,
            today_tokens,
            cloud_count,
            cloud_sum,
        )
    } else {
        route_preview_text(
            &engine,
            config,
            text,
            forced,
            today_tokens,
            cloud_count,
            cloud_sum,
        )
    };
    println!("{out}");
    0
}

/// Print the standard eval report block shared by the built-in and
/// `--external` eval paths.
fn print_eval_report(report: &crate::eval::EvalReport) {
    println!(
        "  accuracy: {:.1}% ({}/{})",
        report.accuracy() * 100.0,
        report.correct,
        report.total
    );
    println!("  cloud rate: {:.1}%", report.cloud_rate() * 100.0);
    println!(
        "  false escalations (local->cloud): {}",
        report.false_escalations
    );
    println!(
        "  missed escalations (cloud->local): {}",
        report.missed_escalations
    );
    println!(
        "  sensitive escalations (I2 breach): {}",
        report.sensitive_escalations
    );
}

/// `pasture eval`: run the routing eval and GATE on it (ADR-271).
///
/// Two corpora, two jobs:
/// - the built-in set (`default_cases`) restates the router's own markers, so
///   it is a **regression check**: anything under 100% means a detector broke;
/// - the held-out set (`holdout_cases`) shares no trigger string with the
///   router, so it **measures generalisation**; it is gated by the recorded
///   floor, not by 100%.
///
/// Exit is non-zero when the built-in set drops below 100%, when held-out
/// accuracy falls below `HOLDOUT_ACCURACY_FLOOR`, or — unconditionally — when
/// any sensitive prompt escalates to cloud (an I2 breach). Before this, every
/// path returned 0 and CI Gate 4 could not fail.
fn run_eval(rest: &[String]) -> i32 {
    let profile = HardwareProfile::detect();
    let engine = RoutingEngine::for_hardware(&profile, true, true);
    // --external <file>: load a user-supplied JSONL eval file and run it
    // through the same routing harness as the built-in cases (IMP-17).
    if let Some(path) = option_value(rest, "--external") {
        match crate::eval::load_eval_cases(path) {
            Ok(cases) => {
                if cases.is_empty() {
                    println!("external eval file contains no cases: {path}");
                    return 0;
                }
                let report = crate::eval::run_eval_owned(&engine, &cases);
                println!(
                    "External routing eval ({} cases from {path}, threshold {}):",
                    report.total,
                    engine.threshold()
                );
                print_eval_report(&report);
            }
            Err(e) => {
                eprintln!("eval --external: {e}");
                return 1;
            }
        }
        return 0;
    }
    let builtin = crate::eval::run_eval(&engine, &crate::eval::default_cases());
    let holdout = crate::eval::run_eval(&engine, &crate::eval::holdout_cases());
    let floor = crate::eval::HOLDOUT_ACCURACY_FLOOR;

    let mut failed = false;
    if builtin.correct < builtin.total {
        failed = true;
    }
    if holdout.accuracy() < floor {
        failed = true;
    }
    let i2_breach = builtin.sensitive_escalations + holdout.sensitive_escalations;
    if i2_breach > 0 {
        failed = true;
    }

    if rest.iter().any(|a| a == "--json") {
        println!(
            "{{\"builtin\":{},\"holdout\":{},\"holdout_floor\":{floor},\"pass\":{}}}",
            builtin.to_json(engine.threshold()),
            holdout.to_json(engine.threshold()),
            !failed
        );
        return i32::from(failed);
    }

    println!(
        "Built-in regression set ({} cases, threshold {}) — restates the marker lists; any miss is a broken detector:",
        builtin.total,
        engine.threshold()
    );
    print_eval_report(&builtin);
    println!(
        "\nHeld-out generalisation set ({} cases) — no prompt contains a trigger string; measures how the rules generalise:",
        holdout.total
    );
    print_eval_report(&holdout);
    println!(
        "  floor: {:.1}% (recorded baseline; the gate fails below it)",
        floor * 100.0
    );
    println!("\nthreshold tuning: `pasture calibrate` fits the threshold to your own cost log");
    if i2_breach > 0 {
        eprintln!("FAIL: {i2_breach} sensitive prompt(s) escalated to cloud — I2 breach");
    }
    if builtin.correct < builtin.total {
        eprintln!("FAIL: built-in regression set below 100% — a detector changed behaviour");
    }
    if holdout.accuracy() < floor {
        eprintln!(
            "FAIL: held-out accuracy {:.1}% fell below the recorded floor {:.1}%",
            holdout.accuracy() * 100.0,
            floor * 100.0
        );
    }
    i32::from(failed)
}

/// `pasture stats`: summarize the PII-free cost log (`--json` for scripting).
fn run_stats(config: &Config, rest: &[String]) -> i32 {
    match crate::cost::read_log(&config.cost_log_path) {
        Ok(recs) => {
            let s = crate::cost::summarize(&recs);
            if rest.iter().any(|a| a == "--json") {
                // Always valid JSON (zeros when the log is empty) for scripting.
                println!("{}", s.to_json());
                return 0;
            }
            if s.total == 0 {
                // ADR-265: an unwritable path also reads as "no records", so
                // saying "run some requests first" would be false — the log will
                // never appear no matter how many run.
                match crate::doctor::cost_log_problem(&config.cost_log_path) {
                    Some(why) => {
                        eprintln!("cost log cannot be written: {why}");
                        eprintln!("set PASTURE_COST_LOG=<path> to a writable location.");
                        return 1;
                    }
                    None => {
                        println!(
                            "no cost log yet at {} (run some requests first)",
                            config.cost_log_path
                        );
                        return 0;
                    }
                }
            }
            println!("Cost log: {} ({} records)", config.cost_log_path, s.total);
            println!(
                "  local: {}  cloud: {}  cache: {}",
                s.local, s.cloud, s.cache
            );
            println!(
                "  cloud rate: {:.1}%   cache hit rate: {:.1}%",
                s.cloud_rate() * 100.0,
                s.cache_rate() * 100.0
            );
            println!(
                "  tokens: prompt {}, completion {}",
                s.prompt_tokens, s.completion_tokens
            );
            println!("  cloud spend: ${:.4}", s.cloud_cost_usd);
            if s.cache > 0 {
                println!("  cache saved {} backend call(s)", s.cache);
            }
            if let Some(lp) = crate::cost::logprob_summary(&recs) {
                println!(
                    "  cascade confidence (mean logprob): n={}, mean {:.3}, min {:.3}, p10 {:.3}, median {:.3}",
                    lp.count, lp.mean, lp.min, lp.p10, lp.median
                );
                println!("  -> tune escalation: pasture calibrate --logprob");
            }
            0
        }
        Err(e) => {
            eprintln!("cannot read cost log: {e}");
            1
        }
    }
}

fn run_chat(config: &Config, text: &str, forced: Option<Route>) -> i32 {
    let profile = HardwareProfile::detect();
    let cloud = make_cloud_backend(config);
    let engine = make_engine(&profile, config, cloud.is_some());
    let report = crate::privacy::classify(text);
    let decision = match engine.decide_with_sensitivity(text, forced, report.is_sensitive()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("routing error: {e}");
            return 1;
        }
    };
    if report.is_sensitive() {
        eprintln!("pasture: sensitive content -> keeping local");
    }

    // Cascade (buffered): try local first, escalate to cloud on low confidence.
    if config.cascade && !report.is_sensitive() && decision.route == Route::Local {
        if let Some(cloud_b) = &cloud {
            let local_b = make_local_backend(config);
            let lreq = chat_request(&config.local_model, text);
            match local_b.complete_scored(&lreq) {
                Ok((lr, confidence)) => {
                    if crate::cascade::should_escalate(
                        &lr.content,
                        confidence,
                        config.cascade_logprob_threshold,
                    ) {
                        match confidence {
                            Some(lp) => eprintln!(
                                "pasture: local mean logprob {lp:.3} < {:.3} -> escalating to cloud",
                                config.cascade_logprob_threshold
                            ),
                            None => eprintln!(
                                "pasture: local answer low-confidence -> escalating to cloud"
                            ),
                        }
                        let creq = chat_request(&config.cloud_model, text);
                        match cloud_b.complete(&creq) {
                            Ok(cr) => println!("{}", cr.content),
                            Err(ce) => {
                                eprintln!(
                                    "pasture: cascade cloud failed ({ce}); using local answer"
                                );
                                println!("{}", lr.content);
                            }
                        }
                    } else {
                        println!("{}", lr.content);
                    }
                    return 0;
                }
                Err(e) => {
                    eprintln!("backend error: {e}");
                    return 1;
                }
            }
        }
    }

    if decision.route == Route::Cloud {
        let Some(backend) = cloud else {
            eprintln!("cloud routing selected but no cloud backend is configured.");
            eprintln!(
                "build with `--features cloud` and set {}.",
                cloud_key_hint(config)
            );
            return 1;
        };
        let req = chat_request(&config.cloud_model, text);
        return match backend.complete(&req) {
            Ok(resp) => {
                println!("{}", resp.content);
                0
            }
            Err(e) => {
                eprintln!("cloud backend error: {e}");
                1
            }
        };
    }

    let backend = make_local_backend(config);
    // Local path streams to the terminal; otherwise identical to chat_request.
    let req = CompletionRequest {
        stream: true,
        ..chat_request(&config.local_model, text)
    };
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let result = backend.stream_complete(&req, &mut |delta| {
        let _ = out.write_all(delta.as_bytes());
        let _ = out.flush();
    });
    match result {
        Ok(_) => {
            let _ = out.write_all(b"\n");
            0
        }
        Err(e) => {
            eprintln!("backend error: {e}");
            eprintln!(
                "(is Ollama running on {}:{}?)",
                config.ollama_host, config.ollama_port
            );
            1
        }
    }
}

/// Build the cloud backend if the `cloud` feature is on and a key is set.
fn make_cloud_backend(config: &Config) -> Option<Box<dyn Backend>> {
    #[cfg(feature = "cloud")]
    {
        let provider = crate::cloud::Provider::from_name(&config.cloud_provider)?;
        let backend = crate::cloud::HttpsCloudBackend::from_env(provider, &config.cloud_model)?
            .with_cache_control(config.cache_control);
        Some(Box::new(backend))
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = config;
        None
    }
}

/// Build the secondary (fallback) cloud backend for multi-provider failover
/// (IMP-9 follow-up). Returns `None` when `cloud_fallback_provider` is unset
/// or the feature flag `cloud` is disabled.
fn make_fallback_cloud_backend(config: &Config) -> Option<Box<dyn Backend>> {
    if config.cloud_fallback_provider.is_empty() {
        return None;
    }
    #[cfg(feature = "cloud")]
    {
        let provider = crate::cloud::Provider::from_name(&config.cloud_fallback_provider)?;
        let model = if config.cloud_fallback_model.is_empty() {
            &config.cloud_model
        } else {
            &config.cloud_fallback_model
        };
        let backend = crate::cloud::HttpsCloudBackend::from_env(provider, model)?;
        Some(Box::new(backend))
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = config;
        None
    }
}

fn cloud_key_hint(config: &Config) -> String {
    match crate::cloud::Provider::from_name(&config.cloud_provider) {
        Some(p) => p.env_key().to_string(),
        None => "the provider API key".to_string(),
    }
}

/// Run environment diagnostics with concrete, copy-pasteable fixes.
/// Print the installed-model list (or the "no models" hint) for a reachable
/// local engine. Shared by the Ollama and OpenAI-compat doctor branches.
fn print_doctor_models(lang: crate::i18n::Lang, models: &[String]) {
    use crate::i18n::{t, tf};
    if models.is_empty() {
        println!("{}", t(lang, "doctor.ollama.nomodels"));
    } else {
        println!(
            "{}",
            tf(
                lang,
                "doctor.ollama.models",
                &[("models", &models.join(", "))]
            )
        );
    }
}

fn run_doctor(config: &Config) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    println!("{}", t(lang, "doctor.title"));
    let mut problems = 0;

    // 1. Is the local engine running?  (Ollama, or an OpenAI server like LM Studio)
    let st = if local_is_openai(config) {
        let (host, p, base) = crate::doctor::parse_base_url(&config.local_openai_url);
        let s = crate::doctor::probe_openai(&host, p, &base);
        let port = p.to_string();
        if s.reachable {
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.engine.ok",
                    &[
                        ("engine", &config.local_backend),
                        ("host", &host),
                        ("port", &port)
                    ]
                )
            );
            print_doctor_models(lang, &s.models);
        } else {
            problems += 1;
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.engine.unreachable",
                    &[
                        ("engine", &config.local_backend),
                        ("host", &host),
                        ("port", &port)
                    ]
                )
            );
            println!("{}", t(lang, "doctor.engine.fix"));
        }
        s
    } else {
        let port = config.ollama_port.to_string();
        let s = crate::doctor::probe_ollama(&config.ollama_host, config.ollama_port);
        if s.reachable {
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.ollama.ok",
                    &[("host", &config.ollama_host), ("port", &port)]
                )
            );
            print_doctor_models(lang, &s.models);
        } else {
            problems += 1;
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.ollama.unreachable",
                    &[("host", &config.ollama_host), ("port", &port)]
                )
            );
            // ADR-265: distinguish "not installed" from "installed but stopped"
            // instead of printing both fixes and making the user guess.
            let key = if crate::doctor::on_path("ollama") {
                "doctor.ollama.fix.notrunning"
            } else {
                "doctor.ollama.fix.notinstalled"
            };
            println!("{}", t(lang, key));
        }
        s
    };

    // 2. Is the configured local model available?
    // ADR-265: when Ollama is down this step used to be SKIPPED, so a bare
    // machine reported "1 item needs attention" when it had two, and the user
    // only learned about the missing model on a second run. Always report it.
    if !st.reachable {
        problems += 1;
        println!(
            "{}",
            tf(
                lang,
                "doctor.model.unknown",
                &[("model", &config.local_model)]
            )
        );
        if !local_is_openai(config) {
            println!(
                "{}",
                tf(lang, "doctor.model.fix", &[("model", &config.local_model)])
            );
        }
    }
    if st.reachable {
        if crate::doctor::has_model(&st.models, &config.local_model) {
            println!(
                "{}",
                tf(lang, "doctor.model.ok", &[("model", &config.local_model)])
            );
        } else {
            problems += 1;
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.model.missing",
                    &[("model", &config.local_model)]
                )
            );
            // The "ollama pull" fix only applies to the Ollama backend.
            if !local_is_openai(config) {
                println!(
                    "{}",
                    tf(lang, "doctor.model.fix", &[("model", &config.local_model)])
                );
            }
        }
    }

    // 3. Is the proxy port free?
    if crate::doctor::port_available(&config.listen_addr) {
        println!(
            "{}",
            tf(lang, "doctor.port.ok", &[("addr", &config.listen_addr)])
        );
    } else {
        problems += 1;
        println!(
            "{}",
            tf(lang, "doctor.port.inuse", &[("addr", &config.listen_addr)])
        );
        println!("{}", t(lang, "doctor.port.fix"));
    }

    // 4. Cloud fallback (optional, informational).
    #[cfg(feature = "cloud")]
    {
        let key = crate::cloud::Provider::from_name(&config.cloud_provider)
            .and_then(crate::cloud::api_key_from_env);
        if key.is_some() {
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.cloud.ready",
                    &[("provider", &config.cloud_provider)]
                )
            );
        } else {
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.cloud.off",
                    &[("hint", &cloud_key_hint(config))]
                )
            );
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        println!("{}", t(lang, "doctor.cloud.localonly"));
    }

    // 5. Can we actually record cost? (ADR-265) Accounting is one of the four
    //    jobs, and it used to fail silently — one stderr line per request.
    match crate::doctor::cost_log_problem(&config.cost_log_path) {
        None => println!(
            "{}",
            tf(
                lang,
                "doctor.costlog.ok",
                &[("path", &config.cost_log_path)]
            )
        ),
        Some(why) => {
            problems += 1;
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.costlog.bad",
                    &[("path", &config.cost_log_path), ("why", &why)]
                )
            );
            println!("{}", t(lang, "doctor.costlog.fix"));
        }
    }

    // 6. Hardware & the routing threshold it drives (ADR-261). This is the
    //    product's differentiator; doctor never used to print it.
    let hw = HardwareProfile::detect();
    let threshold = RoutingEngine::for_hardware(&hw, true, false).threshold();
    match hw.ram_mb {
        Some(mb) => println!(
            "{}",
            tf(
                lang,
                "doctor.hw.ok",
                &[
                    ("ram", &mb.to_string()),
                    ("threshold", &threshold.to_string())
                ]
            )
        ),
        None => {
            // Detection unavailable → routing is running on a best-effort default
            // (lean-local). Tell the user how to make it exact.
            println!(
                "{}",
                tf(
                    lang,
                    "doctor.hw.unknown",
                    &[("threshold", &threshold.to_string())]
                )
            );
            println!("{}", t(lang, "doctor.hw.unknown.fix"));
        }
    }

    println!();
    if problems == 0 {
        println!("{}", t(lang, "doctor.allgood"));
        0
    } else {
        println!(
            "{}",
            tf(lang, "doctor.problems", &[("n", &problems.to_string())])
        );
        1
    }
}

/// Which message `pasture up` should print when Ollama is not answering
/// (ADR-267). Kept pure so both branches are testable without a process.
///
/// `on_path` is whether the `ollama` command exists. The distinction matters:
/// "not installed" and "installed but did not start" have different fixes, and
/// the old code printed the install advice for both — after spawning nothing
/// and waiting 3 s.
fn ollama_unreachable_key(on_path: bool) -> &'static str {
    if on_path {
        "up.ollama_required"
    } else {
        "up.ollama_notinstalled"
    }
}

/// One-command path to running: ensure the model is present (auto-pull if not),
/// then start the proxy. Minimises steps from "Ollama installed" to "serving".
fn run_up(config: &Config, addr: &str) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    print!("{}", t(lang, "up.checking"));

    // OpenAI-compatible local engine (LM Studio, etc.): can't auto-start a GUI
    // server, so just require it to be reachable, then serve.
    if local_is_openai(config) {
        let (host, port, base) = crate::doctor::parse_base_url(&config.local_openai_url);
        let st = crate::doctor::probe_openai(&host, port, &base);
        if !st.reachable {
            eprintln!("{}", t(lang, "up.lmstudio_required"));
            return 1;
        }
        println!("{}", t(lang, "up.starting"));
        return run_serve(config, addr);
    }

    // Ollama must be reachable to pull or serve through. Try to start it.
    let mut st = crate::doctor::probe_ollama(&config.ollama_host, config.ollama_port);
    if !st.reachable {
        // A command that is not on PATH can never be spawned, so say so now
        // instead of spawning nothing, sleeping 3 s and blaming "not running"
        // (ADR-267).
        if !crate::doctor::on_path("ollama") {
            eprintln!("{}", t(lang, ollama_unreachable_key(false)));
            return 1;
        }
        println!("{}", t(lang, "up.starting_ollama"));
        if let Err(e) = std::process::Command::new("ollama")
            .arg("serve")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            eprintln!(
                "{}",
                tf(lang, "up.ollama_spawn_failed", &[("error", &e.to_string())])
            );
            return 1;
        }
        for _ in 0..15 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            st = crate::doctor::probe_ollama(&config.ollama_host, config.ollama_port);
            if st.reachable {
                break;
            }
        }
        if !st.reachable {
            eprintln!("{}", t(lang, ollama_unreachable_key(true)));
            return 1;
        }
        println!("{}", t(lang, "up.ollama_started"));
    }

    // Ensure the local model is available; pull it once if missing.
    if crate::doctor::has_model(&st.models, &config.local_model) {
        println!(
            "{}",
            tf(lang, "up.have_model", &[("model", &config.local_model)])
        );
    } else {
        println!(
            "{}",
            tf(lang, "up.pulling", &[("model", &config.local_model)])
        );
        let status = std::process::Command::new("ollama")
            .arg("pull")
            .arg(&config.local_model)
            .status();
        let ok = matches!(status, Ok(s) if s.success());
        if !ok {
            eprintln!(
                "{}",
                tf(lang, "up.pull_failed", &[("model", &config.local_model)])
            );
            return 1;
        }
    }

    println!("{}", t(lang, "up.starting"));
    run_serve(config, addr)
}

/// Build the per-app connection snippet (pure, testable). None for unknown apps.
fn connect_text(lang: crate::i18n::Lang, app: &str, base: &str) -> Option<String> {
    if app == "lmstudio" {
        return Some(crate::i18n::tf(
            lang,
            "connect.lmstudio",
            &[("base", base), ("base_lm", "http://127.0.0.1:1234/v1")],
        ));
    }
    let key = match app {
        "openwebui" => "connect.openwebui",
        "continue" => "connect.continue",
        "cursor" => "connect.cursor",
        "sdk" => "connect.sdk",
        _ => return None,
    };
    Some(crate::i18n::tf(lang, key, &[("base", base)]))
}

/// Show how to point a given app (or list apps) at the proxy.
fn run_connect(config: &Config, app: Option<&str>) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    let base = format!("http://{}/v1", config.listen_addr);
    match app {
        None => {
            print!("{}", t(lang, "connect.list"));
            println!("{}", connect_text(lang, "sdk", &base).unwrap_or_default());
            0
        }
        Some(a) => match connect_text(lang, a, &base) {
            Some(txt) => {
                println!("{txt}");
                0
            }
            None => {
                eprintln!("{}", tf(lang, "connect.unknown", &[("app", a)]));
                2
            }
        },
    }
}

/// Recommend current local models, highlighting the tier for detected RAM.
fn run_models(_config: &Config) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    let hw = HardwareProfile::detect();
    let no_gpu = hw.gpu.is_none();
    // ADR-261: when RAM is undetected, do NOT steer the user to the CPU-only
    // tier (a 64 GB Mac would otherwise be told to run tinyllama). Assume a mid
    // machine and say the detection failed.
    let (tier, show_ultra, ram_label) = match hw.ram_mb {
        None => ("16 GB", false, "unknown".to_string()),
        Some(ram) if no_gpu && ram < 8_000 => ("4 GB (CPU-only)", true, ram.to_string()),
        Some(ram) if ram < 12_000 => ("8 GB", false, ram.to_string()),
        Some(ram) => ("16 GB", false, ram.to_string()),
    };
    print!("{}", t(lang, "models.title"));
    if hw.ram_mb.is_none() {
        println!("{}", t(lang, "models.hw_unknown"));
    }
    println!(
        "{}",
        tf(
            lang,
            "models.your_machine",
            &[("ram", &ram_label), ("tier", tier)]
        )
    );
    if show_ultra {
        println!("{}", t(lang, "models.ultra"));
        println!("{}", t(lang, "models.local_only_tip"));
    }
    println!("{}", t(lang, "models.low"));
    println!("{}", t(lang, "models.mid"));
    println!("{}", t(lang, "models.multilingual"));
    println!("{}", t(lang, "models.note"));
    0
}

/// `improvements [path] [--review]`: print the self-improvement ledger (IMP-13)
/// — the verified causal record of every change. Defaults to `IMPROVEMENTS.jsonl`.
/// With `--review`, print only the entries the machine approval gate could not
/// auto-approve (IMP-approval-gate), concentrating human review on the minority
/// that needs it.
fn run_improvements(path: Option<&str>, review: bool) -> i32 {
    let path = path.unwrap_or("IMPROVEMENTS.jsonl");
    let items = match crate::improve::read_ledger(path) {
        Ok(items) => items,
        Err(e) => {
            eprintln!("cannot read improvement ledger {path}: {e}");
            return 1;
        }
    };
    if items.is_empty() {
        println!("no improvement ledger at {path} (run from the repo root)");
        return 0;
    }
    let s = crate::improve::summarize(&items);

    if review {
        let (_, needs) = crate::improve::partition_for_review(&items);
        println!(
            "Self-improvement review gate: {} ({} records)",
            path, s.total
        );
        println!(
            "  auto-approved (machine-verified): {}   needs human review: {}   ({:.0}% auto)",
            s.auto_approved,
            s.needs_review,
            s.auto_approval_rate() * 100.0
        );
        if needs.is_empty() {
            println!("\n  nothing needs review — every entry passed the machine gate.");
            return 0;
        }
        println!("\nEntries needing human review:");
        for it in &needs {
            println!(
                "[{}] {} — {} (risk: {})",
                it.status.as_str(),
                it.id,
                it.title,
                it.inferred_risk().as_str()
            );
            if let crate::improve::Approval::NeedsReview(reasons) = it.approval() {
                for r in reasons {
                    println!("    review needed: {r}");
                }
            }
        }
        return 0;
    }

    println!("Self-improvement ledger: {} ({} records)", path, s.total);
    println!(
        "  shipped: {}   deferred: {}   retired: {}",
        s.shipped, s.deferred, s.retired
    );
    println!(
        "  auto-approved: {}   needs review: {}   (pasture improvements --review)",
        s.auto_approved, s.needs_review
    );
    println!();
    for it in &items {
        println!(
            "[{}] {} — {} (risk: {})",
            it.status.as_str(),
            it.id,
            it.title,
            it.inferred_risk().as_str()
        );
        println!("    change: {}", it.change);
        println!("    reason: {}", it.reason);
        println!("    effect: {}", it.effect);
        if !it.grounding.is_empty() {
            println!("    grounding: {}", it.grounding);
        }
    }
    0
}

/// Minimum sample size for a reliable calibration quantile (ADR-172).
/// Below this, the 95% CI for a quantile spans most of the data range and
/// the recommendation is unreliable. The warning is advisory — we still
/// print the threshold so a new user can get started — but the caveat
/// makes the uncertainty explicit.
const MIN_CALIBRATE_SAMPLE: usize = 30;

/// `calibrate [--target <rate>]`: recommend a token threshold from the user's
/// own logged prompt sizes so that ~`rate` of similar prompts route to cloud.
fn run_calibrate(config: &Config, rest: &[String]) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    if rest.iter().any(|a| a == "--error") {
        return run_calibrate_error(lang, rest);
    }
    // IMP-47: validate the signal before trusting any threshold fit to it.
    if rest.iter().any(|a| a == "--auroc") {
        return run_calibrate_auroc(lang, rest);
    }
    let logprob_mode = rest.iter().any(|a| a == "--logprob");
    // IMP-42: --sweep shows several candidate thresholds at once instead of one
    // point chosen by --target. Composes with --logprob to sweep the cascade axis.
    if rest.iter().any(|a| a == "--sweep") {
        return run_calibrate_sweep(config, lang, logprob_mode);
    }
    let target = option_value(rest, "--target")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.2)
        .clamp(0.0, 1.0);
    match crate::cost::read_log(&config.cost_log_path) {
        Ok(records) => {
            if logprob_mode {
                let lps: Vec<f64> = records.iter().filter_map(|r| r.logprob).collect();
                if lps.is_empty() {
                    println!("{}", t(lang, "calibrate.logprob.empty"));
                    return 0;
                }
                let n_lps = lps.len();
                let (threshold, achieved) =
                    crate::calibrate::calibrate_logprob_threshold(&lps, target);
                let n = n_lps.to_string();
                let tgt = format!("{:.0}", target * 100.0);
                let thr = format!("{threshold:.3}");
                let rate = format!("{:.1}", achieved * 100.0);
                println!(
                    "{}",
                    tf(
                        lang,
                        "calibrate.logprob.header",
                        &[("n", &n), ("target", &tgt)]
                    )
                );
                if n_lps < MIN_CALIBRATE_SAMPLE {
                    println!(
                        "{}",
                        tf(
                            lang,
                            "calibrate.small_sample",
                            &[("n", &n), ("min", &MIN_CALIBRATE_SAMPLE.to_string())]
                        )
                    );
                }
                println!(
                    "{}",
                    tf(
                        lang,
                        "calibrate.logprob.result",
                        &[("threshold", &thr), ("rate", &rate)]
                    )
                );
                println!(
                    "{}",
                    tf(lang, "calibrate.logprob.apply", &[("threshold", &thr)])
                );
                return 0;
            }
            let tokens: Vec<u64> = records.iter().map(|r| r.prompt_tokens).collect();
            if tokens.is_empty() {
                println!(
                    "{}",
                    tf(lang, "calibrate.empty", &[("path", &config.cost_log_path)])
                );
                return 0;
            }
            let n_tok = tokens.len();
            let (threshold, achieved) = crate::calibrate::calibrate_threshold(&tokens, target);
            let n = n_tok.to_string();
            let tgt = format!("{:.0}", target * 100.0);
            let thr = threshold.to_string();
            let rate = format!("{:.1}", achieved * 100.0);
            println!(
                "{}",
                tf(lang, "calibrate.header", &[("n", &n), ("target", &tgt)])
            );
            if n_tok < MIN_CALIBRATE_SAMPLE {
                println!(
                    "{}",
                    tf(
                        lang,
                        "calibrate.small_sample",
                        &[("n", &n), ("min", &MIN_CALIBRATE_SAMPLE.to_string())]
                    )
                );
            }
            println!(
                "{}",
                tf(
                    lang,
                    "calibrate.result",
                    &[("threshold", &thr), ("rate", &rate)]
                )
            );
            println!("{}", t(lang, "calibrate.note"));
            println!("{}", tf(lang, "calibrate.apply", &[("threshold", &thr)]));
            0
        }
        Err(e) => {
            eprintln!("cannot read cost log: {e}");
            1
        }
    }
}

/// `calibrate --sweep [--logprob]` (IMP-42): print the recommended threshold at
/// each of several target rates in one table, rather than the single point
/// `--target` picks. RouteLLM's operational lesson is that no threshold is
/// universally right — the correct one depends on the user's own traffic and
/// cost tolerance — so the sweep hands the operator the whole curve to choose
/// from. Advisory and read-only, like every other calibrate mode; reuses the
/// same per-target math (`calibrate::sweep_*`) the `--target` path uses.
fn run_calibrate_sweep(config: &Config, lang: crate::i18n::Lang, logprob_mode: bool) -> i32 {
    use crate::i18n::{t, tf};
    let records = match crate::cost::read_log(&config.cost_log_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot read cost log: {e}");
            return 1;
        }
    };
    let targets = crate::calibrate::DEFAULT_SWEEP_TARGETS;
    if logprob_mode {
        let lps: Vec<f64> = records.iter().filter_map(|r| r.logprob).collect();
        if lps.is_empty() {
            println!("{}", t(lang, "calibrate.logprob.empty"));
            return 0;
        }
        let n = lps.len().to_string();
        println!(
            "{}",
            tf(lang, "calibrate.logprob.sweep.header", &[("n", &n)])
        );
        if lps.len() < MIN_CALIBRATE_SAMPLE {
            println!(
                "{}",
                tf(
                    lang,
                    "calibrate.small_sample",
                    &[("n", &n), ("min", &MIN_CALIBRATE_SAMPLE.to_string())]
                )
            );
        }
        for (target, threshold, achieved) in
            crate::calibrate::sweep_logprob_thresholds(&lps, targets)
        {
            println!(
                "{}",
                tf(
                    lang,
                    "calibrate.logprob.sweep.row",
                    &[
                        ("target", &format!("{:.0}", target * 100.0)),
                        ("threshold", &format!("{threshold:.3}")),
                        ("rate", &format!("{:.1}", achieved * 100.0)),
                    ]
                )
            );
        }
        return 0;
    }
    let tokens: Vec<u64> = records.iter().map(|r| r.prompt_tokens).collect();
    if tokens.is_empty() {
        println!(
            "{}",
            tf(lang, "calibrate.empty", &[("path", &config.cost_log_path)])
        );
        return 0;
    }
    let n = tokens.len().to_string();
    println!("{}", tf(lang, "calibrate.sweep.header", &[("n", &n)]));
    if tokens.len() < MIN_CALIBRATE_SAMPLE {
        println!(
            "{}",
            tf(
                lang,
                "calibrate.small_sample",
                &[("n", &n), ("min", &MIN_CALIBRATE_SAMPLE.to_string())]
            )
        );
    }
    for (target, threshold, achieved) in crate::calibrate::sweep_thresholds(&tokens, targets) {
        println!(
            "{}",
            tf(
                lang,
                "calibrate.sweep.row",
                &[
                    ("target", &format!("{:.0}", target * 100.0)),
                    ("threshold", &threshold.to_string()),
                    ("rate", &format!("{:.1}", achieved * 100.0)),
                ]
            )
        );
    }
    0
}

/// `calibrate --auroc --labels <file>` (IMP-47): measure whether the cascade's
/// confidence signal actually separates correct answers from incorrect ones on
/// *this* machine's model, and say plainly when it does not.
///
/// This is the self-test that has to pass before any other calibrate mode means
/// anything. `--logprob` and `--error` both fit a threshold to the logprob
/// signal; if that signal has no discriminative power, they still return a
/// confident-looking number, and gating on it spends cloud budget at random.
/// Published per-model AUROCs for confidence signals span roughly 0.58 (barely
/// above chance) to 0.84 (genuinely useful) on the same task, so this is
/// model-specific and must be measured, not assumed.
/// `pasture label --prompts <file> [--out <file>]` (ADR-257): build the labelled
/// confidence file that `calibrate --auroc` and `calibrate --error` require.
///
/// Those commands consume `{"logprob":…,"correct":…}` JSONL, but nothing in
/// Pasture produced it — a workflow dead end shipped with ADR-246. It cannot be
/// recovered after the fact either: the cost log records `logprob` but, by
/// design, no prompt or answer text (I3), so there is nothing to review. Labels
/// therefore have to be captured at request time, which is what this does —
/// run each prompt locally, show the answer, record the operator's verdict.
///
/// The output keeps I3: only the score and the verdict are written, never the
/// prompt or the answer.
fn run_label(config: &Config, rest: &[String]) -> i32 {
    use std::io::{BufRead, Write};
    let Some(prompts_path) = option_value(rest, "--prompts") else {
        eprintln!("label needs --prompts <file> (one prompt per line, # comments allowed)");
        return 1;
    };
    let out_path = option_value(rest, "--out").unwrap_or("pasture-labels.jsonl");
    let prompts = match crate::difficulty::load_hard_prompts(prompts_path) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            eprintln!("{prompts_path} has no prompts");
            return 1;
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let backend = make_local_backend(config);
    let mut out = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open {out_path}: {e}");
            return 1;
        }
    };
    println!(
        "Labelling {} prompt(s) with the local backend; appending to {out_path}.",
        prompts.len()
    );
    println!("For each answer: y = correct, n = incorrect, s = skip, q = quit.\n");
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let (mut written, mut unscored) = (0usize, 0usize);
    for (i, prompt) in prompts.iter().enumerate() {
        let req = chat_request(&config.local_model, prompt);
        let (resp, logprob) = match backend.complete_scored(&req) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[{}/{}] backend error: {e}", i + 1, prompts.len());
                continue;
            }
        };
        let Some(lp) = logprob else {
            // Without a score the row is useless to --auroc/--error, so say so
            // rather than writing a row that silently degrades the analysis.
            unscored += 1;
            continue;
        };
        println!("[{}/{}] {}", i + 1, prompts.len(), prompt);
        println!("  -> {}", resp.content.trim());
        print!("  correct? [y/n/s/q] ");
        let _ = std::io::stdout().flush();
        let Some(Ok(answer)) = lines.next() else {
            break;
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" => {
                let _ = writeln!(out, "{{\"logprob\":{lp},\"correct\":true}}");
                written += 1;
            }
            "n" => {
                let _ = writeln!(out, "{{\"logprob\":{lp},\"correct\":false}}");
                written += 1;
            }
            "q" => break,
            _ => {}
        }
    }
    println!("\nWrote {written} label(s) to {out_path}.");
    if unscored > 0 {
        // The default local backend is Ollama, which does not return logprobs,
        // so this is the common case rather than an edge case. Be explicit.
        eprintln!(
            "{unscored} answer(s) had no logprob and were skipped. `calibrate --auroc`/`--error` \
score confidence, so they need a backend that returns logprobs — set \
PASTURE_LOCAL_BACKEND to an OpenAI-compatible server (LM Studio, llama.cpp, vLLM)."
        );
    }
    if written > 0 {
        println!("Next: pasture calibrate --auroc --labels {out_path}");
    }
    0
}

fn run_calibrate_auroc(lang: crate::i18n::Lang, rest: &[String]) -> i32 {
    use crate::calibrate::SignalVerdict;
    use crate::i18n::{t, tf};
    let Some(labels_path) = option_value(rest, "--labels") else {
        eprintln!("{}", t(lang, "calibrate.error.labels-required"));
        return 1;
    };
    let labeled = match crate::calibrate::load_labeled_logprobs(labels_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let Some(q) = crate::calibrate::signal_auroc(&labeled) else {
        // Undefined without both classes — say why rather than printing a number.
        println!(
            "{}",
            tf(lang, "calibrate.auroc.one-class", &[("path", labels_path)])
        );
        return 0;
    };
    let n = labeled.len().to_string();
    println!(
        "{}",
        tf(
            lang,
            "calibrate.auroc.header",
            &[
                ("n", &n),
                ("correct", &q.n_correct.to_string()),
                ("incorrect", &q.n_incorrect.to_string()),
            ]
        )
    );
    println!(
        "{}",
        tf(
            lang,
            "calibrate.auroc.score",
            &[("auroc", &format!("{:.3}", q.auroc))]
        )
    );
    if labeled.len() < MIN_CALIBRATE_SAMPLE {
        println!(
            "{}",
            tf(
                lang,
                "calibrate.small_sample",
                &[("n", &n), ("min", &MIN_CALIBRATE_SAMPLE.to_string())]
            )
        );
    }
    let key = match q.verdict() {
        SignalVerdict::NoBetterThanRandom => "calibrate.auroc.verdict.random",
        SignalVerdict::Weak => "calibrate.auroc.verdict.weak",
        SignalVerdict::Usable => "calibrate.auroc.verdict.usable",
    };
    println!("{}", t(lang, key));
    0
}

/// `calibrate --error --labels <file> [--target <rate>]` (IMP-13, UCCI-style):
/// fit a monotone logprob → error-probability curve on labelled answers and
/// recommend the cascade threshold that keeps local error within the budget.
/// Advisory like the other calibrate modes; the printout shows per-band sample
/// counts so a thin label set is visibly thin.
fn run_calibrate_error(lang: crate::i18n::Lang, rest: &[String]) -> i32 {
    use crate::i18n::{t, tf};
    let Some(labels_path) = option_value(rest, "--labels") else {
        eprintln!("{}", t(lang, "calibrate.error.labels-required"));
        return 1;
    };
    let target = option_value(rest, "--target")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.1)
        .clamp(0.0, 1.0);
    let labeled = match crate::calibrate::load_labeled_logprobs(labels_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let Some(curve) = crate::calibrate::ErrorCurve::fit(&labeled) else {
        println!(
            "{}",
            tf(lang, "calibrate.error.empty", &[("path", labels_path)])
        );
        return 0;
    };
    let n = labeled.len();
    let wrong = labeled.iter().filter(|l| !l.correct).count();
    let overall = format!("{:.1}", wrong as f64 * 100.0 / n as f64);
    let tgt = format!("{:.0}", target * 100.0);
    println!(
        "{}",
        tf(
            lang,
            "calibrate.error.header",
            &[
                ("n", &n.to_string()),
                ("overall", &overall),
                ("target", &tgt)
            ]
        )
    );
    // The fitted step curve: each band runs from its start logprob up to the
    // next band's start (the last band is capped at 0, the logprob maximum).
    let bps = curve.breakpoints();
    for (i, &(from, err, count)) in bps.iter().enumerate() {
        let to = bps.get(i + 1).map_or(0.0, |b| b.0);
        println!(
            "{}",
            tf(
                lang,
                "calibrate.error.band",
                &[
                    ("from", &format!("{from:.3}")),
                    ("to", &format!("{to:.3}")),
                    ("err", &format!("{:.1}", err * 100.0)),
                    ("count", &count.to_string()),
                ]
            )
        );
    }
    match curve.threshold_for_error(target) {
        Some(threshold) => {
            let thr = format!("{threshold:.3}");
            let escalate = labeled.iter().filter(|l| l.logprob < threshold).count();
            let rate = format!("{:.1}", escalate as f64 * 100.0 / n as f64);
            println!(
                "{}",
                tf(
                    lang,
                    "calibrate.error.result",
                    &[("threshold", &thr), ("rate", &rate), ("target", &tgt)]
                )
            );
            println!(
                "{}",
                tf(lang, "calibrate.error.apply", &[("threshold", &thr)])
            );
        }
        None => {
            println!(
                "{}",
                tf(lang, "calibrate.error.unachievable", &[("target", &tgt)])
            );
        }
    }
    0
}

/// `config`: print the effective configuration (resolved env + defaults).
/// API keys are never printed — only whether they are set (I5).
fn run_config(config: &Config) -> i32 {
    use crate::i18n::{detect, t};
    let lang = detect();
    let set = t(lang, "config.set");
    let unset = t(lang, "config.unset");
    let yn = |b: bool| if b { &set } else { &unset };

    let profile = HardwareProfile::detect();
    let hw_default = RoutingEngine::for_hardware(&profile, true, true).threshold();
    // ADR-264: an explicit threshold can now come from the config file as well
    // as PASTURE_THRESHOLD, so don't attribute it to the env var specifically.
    let (threshold, thr_src) = match config.threshold {
        Some(t) => (t, "configured"),
        None => (hw_default, "hardware default"),
    };

    let cloud_key_set = crate::cloud::Provider::from_name(&config.cloud_provider)
        .and_then(crate::cloud::api_key_from_env)
        .is_some();

    println!("{}", t(lang, "config.title"));
    println!("  listen_addr:       {}", config.listen_addr);
    println!("  local_backend:     {}", config.local_backend);
    if local_is_openai(config) {
        println!("  local_openai_url:  {}", config.local_openai_url);
    } else {
        println!(
            "  ollama:            {}:{}",
            config.ollama_host, config.ollama_port
        );
    }
    println!("  local_model:       {}", config.local_model);
    println!("  threshold:         {threshold} tokens ({thr_src})");
    println!("  cloud_provider:    {}", config.cloud_provider);
    println!("  cloud_model:       {}", config.cloud_model);
    println!("  cloud_api_key:     {}", yn(cloud_key_set));
    println!(
        "  allow_sensitive_cloud: {}",
        yn(config.allow_sensitive_cloud)
    );
    println!("  cascade:           {}", yn(config.cascade));
    println!(
        "  cascade_logprob:   {} (escalate below)",
        config.cascade_logprob_threshold
    );
    println!("  cache_size:        {}", config.cache_size);
    println!(
        "  semantic_cache:    {} (threshold {})",
        config.semantic_cache_size, config.semantic_cache_threshold
    );
    println!(
        "  hard_prompts:      {} (threshold {})",
        if config.hard_prompts.is_empty() {
            unset
        } else {
            config.hard_prompts.as_str()
        },
        config.hard_threshold
    );
    if config.skills.is_empty() {
        println!("  skills:            {unset}");
    } else {
        let s: Vec<String> = config
            .skills
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        println!("  skills:            {}", s.join(", "));
    }
    println!("  injection_guard:   {}", config.injection_guard);
    if config.budget_daily_tokens > 0 {
        println!(
            "  budget_daily_tokens: {} (action: {})",
            config.budget_daily_tokens, config.budget_action
        );
        if config.spike_factor > 0 {
            println!("  spike_factor:      {}", config.spike_factor);
        }
    }
    if config.cloud_price_per_1m != (0.0, 0.0) {
        println!(
            "  cloud_price/1M:    ${:.2} in / ${:.2} out",
            config.cloud_price_per_1m.0, config.cloud_price_per_1m.1
        );
    }
    println!("  max_body_bytes:    {}", config.max_body_bytes);
    println!("  cache_control:     {}", config.cache_control);
    println!("  pseudonymize:      {}", config.pseudonymize);
    if !config.otel_log.is_empty() {
        println!("  otel_log:          {}", config.otel_log);
    }
    if !config.cloud_fallback_provider.is_empty() {
        let fb_model = if config.cloud_fallback_model.is_empty() {
            config.cloud_model.as_str()
        } else {
            config.cloud_fallback_model.as_str()
        };
        println!(
            "  cloud_fallback:    {} / {}",
            config.cloud_fallback_provider, fb_model
        );
    }
    println!("  cost_log:          {}", config.cost_log_path);
    println!("  lang:              {}", lang.code());
    0
}

/// Build the routing engine for the current hardware, applying the
/// allow-sensitive-cloud setting and any calibrated threshold override.
fn make_engine(profile: &HardwareProfile, config: &Config, cloud_available: bool) -> RoutingEngine {
    let mut engine = RoutingEngine::for_hardware(profile, true, cloud_available)
        .with_allow_sensitive_cloud(config.allow_sensitive_cloud)
        .with_local_only(config.local_only);
    if let Some(t) = config.threshold {
        engine = engine.with_threshold(t);
    }
    // Skill-profile overrides (IMP-25): convert String route names → Route enum.
    if !config.skills.is_empty() {
        let skills: Vec<(String, crate::routing::Route)> = config
            .skills
            .iter()
            .filter_map(|(skill, route)| {
                let r = match route.as_str() {
                    "local" => crate::routing::Route::Local,
                    "cloud" => crate::routing::Route::Cloud,
                    _ => return None,
                };
                Some((skill.clone(), r))
            })
            .collect();
        engine = engine.with_skills(skills);
    }
    // ADR-256: applies regardless of whether skill overrides are configured.
    engine.with_structured_local(config.structured_local)
}

/// True when the configured local engine is an OpenAI-compatible server.
fn local_is_openai(config: &Config) -> bool {
    matches!(
        config.local_backend.as_str(),
        "lmstudio" | "openai" | "openai-compat"
    )
}

/// Build the local backend (Ollama or an OpenAI-compatible server like LM Studio).
fn make_local_backend(config: &Config) -> Box<dyn Backend> {
    let timeout = std::time::Duration::from_secs(config.local_timeout_secs);
    if local_is_openai(config) {
        let (host, port, base) = crate::doctor::parse_base_url(&config.local_openai_url);
        let path = format!("{base}/chat/completions");
        Box::new(OpenAiCompatBackend::new(
            &host,
            port,
            &path,
            &config.local_model,
            timeout,
        ))
    } else {
        Box::new(OllamaBackend::new(
            &config.ollama_host,
            config.ollama_port,
            &config.local_model,
            timeout,
        ))
    }
}

/// Build a single-user-message, non-streaming chat request.
fn chat_request(model: &str, text: &str) -> CompletionRequest {
    CompletionRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: text.to_string(),
            ..Default::default()
        }],
        stream: false,
        has_tools: false,
        sampling: Default::default(),
    }
}

/// Emit a gentle donation nudge to stderr when due (stdout stays clean for
/// scripting). No-op when disabled or when no donation URL is configured.
/// True when a listen address binds only the loopback interface, so the proxy is
/// reachable only from the same machine (ADR-152). Used to decide whether to warn
/// about an exposed bind without auth. Uses the std parser rather than string
/// prefixes: a prefix check both mis-warns on expanded loopback (`[0:0:…:1]`) and,
/// worse, suppresses the warning for a *global* address that merely starts with a
/// loopback-looking prefix (e.g. `::1:2:3:4`). An unspecified bind (`0.0.0.0`,
/// `::`) is NOT loopback — it listens on every interface and must warn. A hostname
/// that is not a numeric IP is treated as local only when it is literally
/// `localhost`; any other name is assumed exposed (the safe default).
fn listen_addr_is_loopback(addr: &str) -> bool {
    use std::net::{IpAddr, SocketAddr};
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return sa.ip().is_loopback();
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    // Hostname form (`host` or `host:port`). Only a bare-IPv6 string contains a
    // ':' in the host part, and those parse above, so a single trailing ':' here
    // separates a hostname from its port.
    let host = match addr.rfind(':') {
        Some(i) if !addr[..i].contains(':') => &addr[..i],
        _ => addr,
    };
    host.eq_ignore_ascii_case("localhost")
}

fn run_serve(config: &Config, addr: &str) -> i32 {
    let profile = HardwareProfile::detect();
    let cloud = make_cloud_backend(config);
    let engine = make_engine(&profile, config, cloud.is_some());
    let local: Option<Box<dyn Backend>> = Some(make_local_backend(config));
    if cloud.is_some() {
        eprintln!("pasture: cloud backend enabled ({})", config.cloud_provider);
    }
    if config.cache_size > 0 {
        eprintln!(
            "pasture: response cache enabled (cap {})",
            config.cache_size
        );
    }
    // Advertise the configured model ids on GET /v1/models (IMP-8).
    let mut models = vec![config.local_model.clone()];
    if cloud.is_some() {
        models.push(config.cloud_model.clone());
    }
    let fast = if config.local_fast_model.is_empty() {
        None
    } else {
        Some(config.local_fast_model.clone())
    };
    if let Some(ref fm) = fast {
        eprintln!(
            "pasture: fast local model '{}' enabled (threshold {} tokens)",
            fm, config.fast_threshold
        );
    }
    if config.inject_context {
        eprintln!("pasture: context injection enabled (date/OS system message)");
    }
    if config.local_only {
        eprintln!("pasture: local-only mode — cloud backend disabled");
    }
    if config.auth_token.is_some() {
        eprintln!("pasture: bearer-token auth required on /v1/* endpoints");
    }
    if config.rate_limit > 0 {
        eprintln!(
            "pasture: rate limit {} requests/min (global)",
            config.rate_limit
        );
    }
    let cors = crate::proxy::CorsPolicy::parse(&config.cors_origins);
    if cors.is_some() {
        eprintln!("pasture: CORS enabled for origins: {}", config.cors_origins);
    }
    // Embedding difficulty signal (IMP-14): load known-hard prompts if configured.
    // A bad path warns loudly but does not stop the server (the signal is advisory;
    // the deterministic routing baseline is unaffected).
    let hard_prompts = if config.hard_prompts.is_empty() {
        Vec::new()
    } else {
        match crate::difficulty::load_hard_prompts(&config.hard_prompts) {
            Ok(p) => {
                eprintln!(
                    "pasture: difficulty signal enabled: {} hard prompt(s) (threshold {})",
                    p.len(),
                    config.hard_threshold
                );
                p
            }
            Err(e) => {
                eprintln!("pasture: WARNING — difficulty signal disabled: {e}");
                Vec::new()
            }
        }
    };
    // Security nudge: a non-loopback bind without auth is exposed to the network.
    // Authoritative loopback detection (ADR-152), not a string-prefix guess.
    if !listen_addr_is_loopback(addr) && config.auth_token.is_none() {
        eprintln!(
            "pasture: WARNING — listening on {addr} (non-localhost) without auth; \
             set PASTURE_AUTH_TOKEN to require a bearer token"
        );
    }
    let proxy = Proxy::new(engine, local, cloud, &config.cost_log_path)
        .with_cascade(config.cascade)
        .with_cascade_logprob(config.cascade_logprob_threshold)
        .with_cache(config.cache_size)
        // ADR-229: with_cache_ttl and with_semantic_cache are now
        // order-independent (the TTL is stored and applied to whichever cache
        // is configured first or second), so this ordering is no longer load-
        // bearing — kept as-is to minimize diff.
        .with_semantic_cache(config.semantic_cache_size, config.semantic_cache_threshold)
        .with_semantic_min_lexical(config.semantic_cache_min_lexical)
        .with_cache_ttl(config.cache_ttl_secs)
        .with_hard_prompts(hard_prompts, config.hard_threshold)
        .with_models(models)
        .with_cloud_retry(config.cloud_retry)
        .with_fast_model(fast, config.fast_threshold)
        .with_inject_context(config.inject_context)
        .with_system_prompt(if config.system_prompt.is_empty() {
            None
        } else {
            Some(config.system_prompt.clone())
        })
        .with_model_names(config.local_model.clone(), config.cloud_model.clone())
        .with_access_log(if config.access_log.is_empty() {
            None
        } else {
            Some(config.access_log.clone())
        })
        .with_auth_token(config.auth_token.clone())
        .with_rate_limit(config.rate_limit)
        .with_cors(cors)
        .with_request_timeout(if config.request_timeout_secs > 0 {
            Some(std::time::Duration::from_secs(config.request_timeout_secs))
        } else {
            None
        })
        .with_injection_guard(&config.injection_guard)
        .with_budget(
            config.budget_daily_tokens,
            &config.budget_action,
            config.spike_factor,
            &config.cost_log_path,
        )
        .with_cloud_price(config.cloud_price_per_1m.0, config.cloud_price_per_1m.1)
        .with_max_body_bytes(config.max_body_bytes)
        .with_pseudonymize(config.pseudonymize)
        .with_output_pii_scan(config.output_pii_scan)
        .with_input_pii_scan(config.input_pii_scan)
        .with_decision_log(if config.decision_log.is_empty() {
            None
        } else {
            Some(config.decision_log.as_str())
        })
        .with_health_cooldown(config.health_cooldown_secs)
        .with_otel_log(if config.otel_log.is_empty() {
            None
        } else {
            Some(config.otel_log.clone())
        })
        .with_cloud_system(&config.cloud_provider)
        .with_cloud_fallback(make_fallback_cloud_backend(config));
    // ADR-262: check the port BEFORE printing "you're connected". Previously the
    // success banner was printed unconditionally and `serve` then died with a raw
    // `os error 98`, so a user with 8645 occupied saw a happy connect message
    // followed by a cryptic failure. `doctor` already had an actionable fix
    // string for exactly this case; `serve`/`up` never reached it.
    let lang = crate::i18n::detect();
    if !crate::doctor::port_available(addr) {
        eprintln!(
            "{}",
            crate::i18n::tf(lang, "doctor.port.inuse", &[("addr", addr)])
        );
        eprintln!("{}", crate::i18n::t(lang, "doctor.port.fix"));
        return 1;
    }
    print!(
        "{}",
        crate::i18n::tf(lang, "connect.help", &[("addr", addr)])
    );
    match proxy.serve(addr) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("server error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::i18n::{t, Lang};

    /// ADR-267: `up` must distinguish "no `ollama` command" from "it did not
    /// start". Before this, both printed the install advice, so a user whose
    /// Ollama was installed-but-wedged was told to install it again.
    #[test]
    fn test_ollama_unreachable_key_splits_by_path_presence() {
        assert_eq!(ollama_unreachable_key(false), "up.ollama_notinstalled");
        assert_eq!(ollama_unreachable_key(true), "up.ollama_required");
        for lang in [Lang::En, Lang::Ja] {
            let missing = t(lang, ollama_unreachable_key(false));
            let wedged = t(lang, ollama_unreachable_key(true));
            assert_ne!(missing, wedged, "the two cases must read differently");
            // The actionable half of each: install vs. start.
            assert!(missing.contains("https://ollama.com"), "{missing}");
            assert!(wedged.contains("ollama serve"), "{wedged}");
        }
    }

    /// The messages `up` reaches for must all exist in both catalogs; `t()`
    /// falls back to the key itself when one is missing, which would ship the
    /// raw key to the user.
    #[test]
    fn test_up_ollama_messages_are_translated() {
        for key in [
            "up.ollama_required",
            "up.ollama_notinstalled",
            "up.ollama_spawn_failed",
        ] {
            for lang in [Lang::En, Lang::Ja] {
                assert_ne!(t(lang, key), key, "{key} missing from a catalog");
            }
        }
    }

    #[test]
    fn test_serve_refuses_occupied_port_without_success_banner() {
        // ADR-262: `serve` used to print the "you're connected" banner and only
        // then fail with a raw `os error 98`. Binding the port first proves the
        // pre-check fires and returns non-zero before any success output.
        use std::net::TcpListener;
        let held = TcpListener::bind("127.0.0.1:0").expect("bind probe port");
        let addr = held.local_addr().unwrap().to_string();
        // Port is held by `held` for the duration of this call.
        assert!(
            !crate::doctor::port_available(&addr),
            "held port must read as unavailable"
        );
        let cfg = Config::default();
        assert_eq!(
            run_serve(&cfg, &addr),
            1,
            "serve must refuse an occupied port rather than claiming success"
        );
    }

    #[test]
    fn test_label_requires_prompts_flag() {
        // ADR-257: without --prompts there is nothing to label; fail loudly
        // rather than silently producing an empty labels file.
        let cfg = Config::default();
        assert_eq!(run_label(&cfg, &[]), 1);
    }

    #[test]
    fn test_label_rejects_missing_or_empty_prompt_file() {
        let cfg = Config::default();
        assert_eq!(
            run_label(
                &cfg,
                &["--prompts".to_string(), "/no/such/file".to_string()]
            ),
            1
        );
        // A file of only comments/blank lines has no prompts.
        let path = std::env::temp_dir().join("pasture_label_empty_test.txt");
        std::fs::write(&path, "# just a comment\n\n").unwrap();
        assert_eq!(
            run_label(
                &cfg,
                &["--prompts".to_string(), path.to_string_lossy().to_string()]
            ),
            1
        );
        let _ = std::fs::remove_file(&path);
    }
    use super::*;
    use crate::hardware::GpuInfo;

    #[test]
    fn test_listen_addr_is_loopback_authoritative() {
        // IPv4 loopback block and IPv6 ::1 (bracketed, with port) are local.
        assert!(listen_addr_is_loopback("127.0.0.1:11435"));
        assert!(listen_addr_is_loopback("127.5.6.7:80")); // all of 127/8
        assert!(listen_addr_is_loopback("[::1]:11435"));
        assert!(listen_addr_is_loopback("::1")); // bare, no port
        assert!(listen_addr_is_loopback("localhost:11435"));
        assert!(listen_addr_is_loopback("localhost"));
        // Expanded IPv6 loopback must NOT be mis-flagged as exposed. The old
        // string-prefix check returned false here (no "::1"/"[::1]" prefix match),
        // wrongly warning that a loopback-only bind was network-exposed.
        assert!(listen_addr_is_loopback("[0:0:0:0:0:0:0:1]:8080"));
        // Exposed binds are not loopback → the warning must fire.
        assert!(!listen_addr_is_loopback("0.0.0.0:11435")); // all IPv4 interfaces
        assert!(!listen_addr_is_loopback("[::]:11435")); // all IPv6 interfaces
        assert!(!listen_addr_is_loopback("192.168.1.5:11435")); // LAN
        assert!(!listen_addr_is_loopback("myhost.lan:8080")); // hostname, not localhost
                                                              // Regression: a GLOBAL address that merely starts with the "::1" prefix
                                                              // must NOT be treated as loopback. The old heuristic's
                                                              // `starts_with("::1")` returned true for this and SUPPRESSED the
                                                              // exposed-without-auth warning; `::1:2:3:4` is `0:0:0:0:1:2:3:4`, global.
        assert!(!listen_addr_is_loopback("::1:2:3:4"));
    }

    #[test]
    fn test_parse_route_flag_cloud() {
        let args = vec!["--cloud".to_string()];
        assert_eq!(parse_route_flag(&args), Some(Route::Cloud));
    }

    #[test]
    fn test_parse_route_flag_local() {
        let args = vec!["--local".to_string()];
        assert_eq!(parse_route_flag(&args), Some(Route::Local));
    }

    #[test]
    fn test_parse_route_flag_none() {
        let args = vec!["hello".to_string()];
        assert_eq!(parse_route_flag(&args), None);
    }

    #[test]
    fn test_positional_filters_flags() {
        let args = vec!["text".to_string(), "--local".to_string()];
        assert_eq!(positional(&args), vec!["text"]);
    }

    #[test]
    fn test_option_value_reads_following_arg() {
        let args = vec!["--addr".to_string(), "0.0.0.0:9".to_string()];
        assert_eq!(option_value(&args, "--addr"), Some("0.0.0.0:9"));
    }

    #[test]
    fn test_option_value_missing() {
        let args = vec!["--addr".to_string()];
        assert_eq!(option_value(&args, "--addr"), None);
    }

    #[test]
    fn test_route_decision_text_contains_route() {
        let engine = RoutingEngine::new(100, true, true);
        let out = route_decision_text(&engine, "hi", None);
        assert!(out.contains("route: local"));
        assert!(out.contains("threshold: 100"));
    }

    #[test]
    fn test_hardware_text_with_gpu() {
        let p = HardwareProfile {
            ram_mb: Some(16000),
            cpu_count: 8,
            gpu: Some(GpuInfo {
                vendor: "nvidia".into(),
                vram_mb: Some(8192),
            }),
        };
        let out = hardware_text(&p);
        assert!(out.contains("RAM: 16000 MB"));
        assert!(out.contains("8192 MB VRAM"));
    }

    #[test]
    fn test_hardware_text_unknown_ram_says_unknown() {
        // ADR-261: `pasture hw` used to print "RAM: 0 MB" on macOS/Windows,
        // which reads as a real (tiny) machine rather than a failed probe.
        let p = HardwareProfile {
            ram_mb: None,
            cpu_count: 10,
            gpu: None,
        };
        let out = hardware_text(&p);
        assert!(out.contains("unknown"), "must say unknown: {out}");
        assert!(
            !out.contains("0 MB"),
            "must not imply a 0 MB machine: {out}"
        );
    }

    #[test]
    fn test_hardware_text_without_gpu() {
        let p = HardwareProfile {
            ram_mb: Some(8000),
            cpu_count: 4,
            gpu: None,
        };
        assert!(hardware_text(&p).contains("GPU: none"));
    }

    #[test]
    fn test_run_version_returns_zero() {
        let args = vec!["pasture".to_string(), "version".to_string()];
        assert_eq!(run(&args), 0);
    }

    #[test]
    fn test_run_unknown_returns_two() {
        let args = vec!["pasture".to_string(), "bogus".to_string()];
        assert_eq!(run(&args), 2);
    }

    #[test]
    fn test_run_no_command_shows_welcome() {
        // No command prints a friendly welcome and exits 0 (beginner-friendly).
        let args = vec!["pasture".to_string()];
        assert_eq!(run(&args), 0);
    }

    #[test]
    fn test_run_help_returns_zero() {
        assert_eq!(run(&["pasture".to_string(), "help".to_string()]), 0);
    }

    #[test]
    fn test_connect_text_known_apps() {
        use crate::i18n::Lang;
        let base = "http://127.0.0.1:8645/v1";
        for app in ["openwebui", "continue", "cursor", "sdk"] {
            let txt = connect_text(Lang::En, app, base).unwrap();
            assert!(txt.contains(base), "{app}: {txt}");
        }
        assert!(connect_text(Lang::En, "nope", base).is_none());
    }

    #[test]
    fn test_connect_text_localized() {
        use crate::i18n::Lang;
        let txt = connect_text(Lang::Ja, "cursor", "http://x/v1").unwrap();
        assert!(txt.contains("http://x/v1"));
    }

    // --- route_preview_text tests (ADR-201) ---

    fn preview_engine() -> RoutingEngine {
        RoutingEngine::new(300, true, true)
    }

    fn base_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_route_preview_local_shows_token_estimate() {
        let engine = preview_engine();
        let cfg = base_config();
        // Short text → local; must show token estimate and $0 cost line.
        let out = route_preview_text(&engine, &cfg, "hi", None, 0, 0, 0);
        assert!(out.contains("route:     local"), "{out}");
        assert!(out.contains("estimated:"), "{out}");
        assert!(out.contains("cost:      $0.000000 (local"), "{out}");
    }

    #[test]
    fn test_route_preview_cloud_shows_no_price_hint() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.cloud_price_per_1m = (0.0, 0.0);
        // Force cloud so we see the cost line regardless of text length.
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 0, 0, 0);
        assert!(out.contains("route:     cloud"), "{out}");
        assert!(out.contains("no price configured"), "{out}");
        assert!(out.contains("budget:    not configured"), "{out}");
    }

    #[test]
    fn test_route_preview_cloud_with_price() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.cloud_price_per_1m = (1.0, 5.0);
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 0, 0, 0);
        assert!(out.contains("cost:      $"), "{out}");
        assert!(out.contains("$1.00/$5.00 per 1M"), "{out}");
    }

    #[test]
    fn test_route_preview_budget_within() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 10_000;
        cfg.budget_action = "block".to_string();
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 500, 2, 1000);
        assert!(out.contains("within budget"), "{out}");
        assert!(out.contains("500/10000"), "{out}");
    }

    #[test]
    fn test_route_preview_budget_exceeded_block() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 1_000;
        cfg.budget_action = "block".to_string();
        // today_tokens already at limit
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 1_000, 1, 1000);
        assert!(out.contains("would be blocked"), "{out}");
    }

    #[test]
    fn test_route_preview_budget_exceeded_redirect() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 1_000;
        cfg.budget_action = "local-only".to_string();
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 1_200, 1, 1200);
        assert!(out.contains("would redirect to local"), "{out}");
    }

    #[test]
    fn test_route_preview_spike_detected() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.spike_factor = 5;
        cfg.budget_action = "local-only".to_string();
        // avg = 100 tokens; spike_factor=5 → threshold 500.  Predicted total for
        // a 800-token text is ~1200 (> 500) → spike.
        let long_text = "word ".repeat(800);
        let out = route_preview_text(
            &engine,
            &cfg,
            &long_text,
            Some(Route::Cloud),
            0,
            10,   // cloud_count
            1000, // cloud_sum → avg = 100
        );
        assert!(out.contains("spike:"), "{out}");
    }

    #[test]
    fn test_route_preview_sensitive_shown() {
        let engine = preview_engine();
        let cfg = base_config();
        let out = route_preview_text(&engine, &cfg, "my password is hunter2", None, 0, 0, 0);
        assert!(out.contains("sensitive: yes"), "{out}");
    }

    #[test]
    fn test_route_preview_no_budget_line_for_local() {
        let engine = preview_engine();
        let mut cfg = base_config();
        // Even with budget configured, a local route should not print a budget line.
        cfg.budget_daily_tokens = 5_000;
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Local), 0, 0, 0);
        assert!(!out.contains("budget:"), "{out}");
    }

    #[test]
    fn test_route_preview_redirect_shows_effective_local_route_and_zero_cost() {
        // ADR-202 semantic fix: a budget redirect must show the EFFECTIVE route
        // (local) and $0 cost, not the pre-guard engine route (cloud) with a
        // cloud cost. Matches the HTTP endpoint and reality.
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 1_000;
        cfg.budget_action = "local-only".to_string();
        cfg.cloud_price_per_1m = (10.0, 30.0);
        let out = route_preview_text(&engine, &cfg, "hi", Some(Route::Cloud), 2_000, 1, 2_000);
        assert!(out.contains("route:     local"), "{out}");
        assert!(out.contains("$0.000000"), "{out}");
        assert!(out.contains("would redirect to local"), "{out}");
    }

    // --- route_preview_json: format parity with HTTP POST /v1/route (ADR-202) ---

    fn parse_json(s: &str) -> crate::json::JsonValue {
        crate::json::parse(s).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{s}"))
    }

    #[test]
    fn test_route_preview_json_is_valid_pasture_route_object() {
        let engine = preview_engine();
        let cfg = base_config();
        let out = route_preview_json(&engine, &cfg, "hi", None, 0, 0, 0);
        let v = parse_json(&out);
        assert_eq!(
            v.get("object").and_then(|x| x.as_str()),
            Some("pasture.route")
        );
        assert_eq!(v.get("route").and_then(|x| x.as_str()), Some("local"));
        assert_eq!(v.get("has_tools").and_then(|x| x.as_bool()), Some(false));
        // budget is null when no guard is active.
        assert!(matches!(
            v.get("budget"),
            Some(crate::json::JsonValue::Null)
        ));
    }

    #[test]
    fn test_route_preview_json_has_all_http_fields() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.cloud_price_per_1m = (1.0, 5.0);
        let out = route_preview_json(&engine, &cfg, "hi", Some(Route::Cloud), 0, 0, 0);
        let v = parse_json(&out);
        for field in [
            "object",
            "route",
            "reason",
            "budget",
            "sensitive",
            "categories",
            "estimated_tokens",
            "predicted_output_tokens",
            "predicted_total_tokens",
            "estimated_cost_usd",
            "has_tools",
        ] {
            assert!(v.get(field).is_some(), "missing {field} in {out}");
        }
        // Cloud route with pricing → positive cost.
        assert!(
            v.get("estimated_cost_usd")
                .and_then(|x| x.as_f64())
                .unwrap()
                > 0.0
        );
    }

    #[test]
    fn test_route_preview_json_redirect_route_is_local_with_budget_string() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 1_000;
        cfg.budget_action = "local-only".to_string();
        cfg.cloud_price_per_1m = (10.0, 30.0);
        let out = route_preview_json(&engine, &cfg, "hi", Some(Route::Cloud), 2_000, 1, 2_000);
        let v = parse_json(&out);
        // Effective route is local; cost is 0; budget explains the redirect.
        assert_eq!(v.get("route").and_then(|x| x.as_str()), Some("local"));
        assert_eq!(
            v.get("estimated_cost_usd").and_then(|x| x.as_f64()),
            Some(0.0)
        );
        let budget = v.get("budget").and_then(|x| x.as_str()).unwrap_or("");
        assert!(budget.contains("redirected to local"), "{out}");
    }

    #[test]
    fn test_route_preview_json_sensitive_categories_are_labels() {
        let engine = preview_engine();
        let cfg = base_config();
        let out = route_preview_json(&engine, &cfg, "my password is hunter2", None, 0, 0, 0);
        let v = parse_json(&out);
        assert_eq!(v.get("sensitive").and_then(|x| x.as_bool()), Some(true));
        // The raw secret value must never appear (I3: labels only).
        assert!(!out.contains("hunter2"), "{out}");
    }

    #[test]
    fn test_route_preview_json_block_keeps_cloud_route_zero_cost() {
        let engine = preview_engine();
        let mut cfg = base_config();
        cfg.budget_daily_tokens = 1_000;
        cfg.budget_action = "block".to_string();
        cfg.cloud_price_per_1m = (10.0, 30.0);
        let out = route_preview_json(&engine, &cfg, "hi", Some(Route::Cloud), 1_000, 1, 1_000);
        let v = parse_json(&out);
        // block keeps the cloud route but won't serve it → cost 0, budget note set.
        assert_eq!(v.get("route").and_then(|x| x.as_str()), Some("cloud"));
        assert_eq!(
            v.get("estimated_cost_usd").and_then(|x| x.as_f64()),
            Some(0.0)
        );
        let budget = v.get("budget").and_then(|x| x.as_str()).unwrap_or("");
        assert!(budget.contains("would be blocked"), "{out}");
    }
}

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
    route <text>             Dry-run: show where a prompt would be routed
    chat  <text>             Send one prompt through the router (needs Ollama)
    serve                    Start the OpenAI-compatible proxy server
    eval                     Measure routing quality + token-threshold sweep
    stats                    Summarize the cost log (routes, tokens, spend)
    config                   Print the effective configuration (no secrets)
    calibrate [--target R]   Recommend PASTURE_THRESHOLD from your logged usage (R=cloud rate, default 0.2)
    calibrate --logprob       Recommend PASTURE_CASCADE_LOGPROB from logged cascade confidence
    donate                   Show how to support development ($1/month)
    refer [provider]         Show configurable cloud-provider referral links
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

/// Format the hardware profile for display (testable).
pub fn hardware_text(p: &HardwareProfile) -> String {
    let gpu = match &p.gpu {
        Some(g) => match g.vram_mb {
            Some(v) => format!("{} ({v} MB VRAM)", g.vendor),
            None => g.vendor.clone(),
        },
        None => "none".to_string(),
    };
    format!(
        "RAM: {} MB\nCPU threads: {}\nGPU: {}",
        p.ram_mb, p.cpu_count, gpu
    )
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
    let config = Config::default().with_env();

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
        "route" => {
            let pos = positional(rest);
            let Some(text) = pos.first() else {
                eprintln!("usage: pasture route <text> [--local|--cloud]");
                return 2;
            };
            // Dry-run shows the full rule logic, assuming both backends present.
            let profile = HardwareProfile::detect();
            let engine = make_engine(&profile, &config, true);
            let report = crate::privacy::classify(text);
            if report.is_sensitive() {
                println!("sensitive: yes [{}]", report.categories.join(", "));
            } else {
                println!("sensitive: no");
            }
            match engine.decide_with_sensitivity(
                text,
                parse_route_flag(rest),
                report.is_sensitive(),
            ) {
                Ok(d) => println!(
                    "route: {}\nreason: {}\nthreshold: {} tokens",
                    d.route.as_str(),
                    d.reason,
                    engine.threshold()
                ),
                Err(e) => println!("error: {e}"),
            }
            0
        }
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
        "eval" => {
            let profile = HardwareProfile::detect();
            let engine = RoutingEngine::for_hardware(&profile, true, true);
            let report = crate::eval::run_eval(&engine, &crate::eval::default_cases());
            println!(
                "Routing eval ({} cases, threshold {}):",
                report.total,
                engine.threshold()
            );
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
            println!("\nThreshold sweep (plain prompts, cloud rate):");
            for (t, rate) in
                crate::eval::sweep(&crate::eval::length_samples(), &[50, 100, 300, 800, 2000])
            {
                println!("  thr {t:>5}: {:.0}%", rate * 100.0);
            }
            0
        }
        "stats" => match crate::cost::read_log(&config.cost_log_path) {
            Ok(recs) => {
                let s = crate::cost::summarize(&recs);
                if s.total == 0 {
                    println!(
                        "no cost log yet at {} (run some requests first)",
                        config.cost_log_path
                    );
                    return 0;
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
        },
        "calibrate" => run_calibrate(&config, rest),
        "config" => run_config(&config),
        "donate" => {
            println!(
                "{}",
                crate::monetize::donation_text(config.donate_url.as_deref())
            );
            0
        }
        "refer" => {
            let pos = positional(rest);
            if let Some(key) = pos.first() {
                match crate::monetize::provider(key) {
                    Some(p) => {
                        match crate::monetize::referral_url(p.key, env_referral_resolver) {
                            Some(u) => println!("{}: {u}", p.display),
                            None => println!(
                                "{}: not configured. Set PASTURE_REF_{}=<your affiliate url>\n({})",
                                p.display,
                                p.key.to_uppercase(),
                                p.homepage
                            ),
                        }
                        0
                    }
                    None => {
                        eprintln!("unknown provider: {key}");
                        2
                    }
                }
            } else {
                println!(
                    "{}",
                    crate::monetize::referral_list_text(env_referral_resolver)
                );
                0
            }
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print!("{USAGE}");
            2
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
                            Err(_) => println!("{}", lr.content), // fallback to local
                        }
                    } else {
                        println!("{}", lr.content);
                    }
                    maybe_nudge(config);
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
            eprintln!("to compare cloud providers: pasture refer");
            return 1;
        };
        let req = CompletionRequest {
            model: config.cloud_model.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                content: text.to_string(),
            }],
            stream: false,
        };
        return match backend.complete(&req) {
            Ok(resp) => {
                println!("{}", resp.content);
                maybe_nudge(config);
                0
            }
            Err(e) => {
                eprintln!("cloud backend error: {e}");
                1
            }
        };
    }

    let backend = make_local_backend(config);
    let req = CompletionRequest {
        model: config.local_model.clone(),
        messages: vec![Message {
            role: "user".to_string(),
            content: text.to_string(),
        }],
        stream: true,
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
            maybe_nudge(config);
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
        let backend = crate::cloud::HttpsCloudBackend::from_env(provider, &config.cloud_model)?;
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
            if s.models.is_empty() {
                println!("{}", t(lang, "doctor.ollama.nomodels"));
            } else {
                println!(
                    "{}",
                    tf(
                        lang,
                        "doctor.ollama.models",
                        &[("models", &s.models.join(", "))]
                    )
                );
            }
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
            if s.models.is_empty() {
                println!("{}", t(lang, "doctor.ollama.nomodels"));
            } else {
                println!(
                    "{}",
                    tf(
                        lang,
                        "doctor.ollama.models",
                        &[("models", &s.models.join(", "))]
                    )
                );
            }
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
            println!("{}", t(lang, "doctor.ollama.fix"));
        }
        s
    };

    // 2. Is the configured local model available?
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
        println!("{}", t(lang, "up.starting_ollama"));
        let _ = std::process::Command::new("ollama")
            .arg("serve")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        for _ in 0..15 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            st = crate::doctor::probe_ollama(&config.ollama_host, config.ollama_port);
            if st.reachable {
                break;
            }
        }
        if !st.reachable {
            eprintln!("{}", t(lang, "up.ollama_required"));
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
    let ram = HardwareProfile::detect().ram_mb;
    let tier = if ram < 12_000 { "8 GB" } else { "16 GB" };
    print!("{}", t(lang, "models.title"));
    println!(
        "{}",
        tf(
            lang,
            "models.your_machine",
            &[("ram", &ram.to_string()), ("tier", tier)]
        )
    );
    println!("{}", t(lang, "models.low"));
    println!("{}", t(lang, "models.mid"));
    println!("{}", t(lang, "models.multilingual"));
    println!("{}", t(lang, "models.note"));
    0
}

/// `calibrate [--target <rate>]`: recommend a token threshold from the user's
/// own logged prompt sizes so that ~`rate` of similar prompts route to cloud.
fn run_calibrate(config: &Config, rest: &[String]) -> i32 {
    use crate::i18n::{detect, t, tf};
    let lang = detect();
    let logprob_mode = rest.iter().any(|a| a == "--logprob");
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
                let (threshold, achieved) =
                    crate::calibrate::calibrate_logprob_threshold(&lps, target);
                let n = lps.len().to_string();
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
            let (threshold, achieved) = crate::calibrate::calibrate_threshold(&tokens, target);
            let n = tokens.len().to_string();
            let tgt = format!("{:.0}", target * 100.0);
            let thr = threshold.to_string();
            let rate = format!("{:.1}", achieved * 100.0);
            println!(
                "{}",
                tf(lang, "calibrate.header", &[("n", &n), ("target", &tgt)])
            );
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
    let (threshold, thr_src) = match config.threshold {
        Some(t) => (t, "PASTURE_THRESHOLD"),
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
    println!("  cost_log:          {}", config.cost_log_path);
    println!("  donate_url:        {}", yn(config.donate_url.is_some()));
    println!("  lang:              {}", lang.code());
    0
}

/// Build the routing engine for the current hardware, applying the
/// allow-sensitive-cloud setting and any calibrated threshold override.
fn make_engine(profile: &HardwareProfile, config: &Config, cloud_available: bool) -> RoutingEngine {
    let mut engine = RoutingEngine::for_hardware(profile, true, cloud_available)
        .with_allow_sensitive_cloud(config.allow_sensitive_cloud);
    if let Some(t) = config.threshold {
        engine = engine.with_threshold(t);
    }
    engine
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
    if local_is_openai(config) {
        let (host, port, base) = crate::doctor::parse_base_url(&config.local_openai_url);
        let path = format!("{base}/chat/completions");
        Box::new(OpenAiCompatBackend::new(
            &host,
            port,
            &path,
            &config.local_model,
        ))
    } else {
        Box::new(OllamaBackend::new(
            &config.ollama_host,
            config.ollama_port,
            &config.local_model,
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
        }],
        stream: false,
    }
}

/// Resolve a referral URL from `PASTURE_REF_<KEY>` (key uppercased).
fn env_referral_resolver(key: &str) -> Option<String> {
    std::env::var(format!("PASTURE_REF_{}", key.to_uppercase())).ok()
}

/// Emit a gentle donation nudge to stderr when due (stdout stays clean for
/// scripting). No-op when disabled or when no donation URL is configured.
fn maybe_nudge(config: &Config) {
    if config.no_nudge {
        return;
    }
    let Some(url) = config.donate_url.as_deref() else {
        return;
    };
    let count = crate::monetize::bump_count(&config.state_path);
    if crate::monetize::should_nudge(count, crate::monetize::NUDGE_EVERY) {
        if let Some(msg) = crate::monetize::nudge_text(Some(url)) {
            eprintln!("{msg}");
        }
    }
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
    let proxy = Proxy::new(engine, local, cloud, &config.cost_log_path)
        .with_cascade(config.cascade)
        .with_cascade_logprob(config.cascade_logprob_threshold)
        .with_cache(config.cache_size);
    print!(
        "{}",
        crate::i18n::tf(crate::i18n::detect(), "connect.help", &[("addr", addr)])
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
    use super::*;
    use crate::hardware::GpuInfo;

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
            ram_mb: 16000,
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
    fn test_hardware_text_without_gpu() {
        let p = HardwareProfile {
            ram_mb: 8000,
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
}

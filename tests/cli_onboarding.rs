//! Onboarding behaviour, pinned at the binary boundary (ADR-268 / audit A11).
//!
//! ADR-261..267 fixed seven first-run defects, each verified once by hand
//! against a release build. Hand verification does not survive the next commit:
//! `run_doctor` / `run_up` / `run_stats` / `run_models` print and spawn, so no
//! unit test reached them and every one of those fixes was a regression waiting
//! to happen. These tests run the real binary in a **cleared environment** —
//! empty `PATH` (so `ollama` is genuinely absent), throwaway `HOME`, no
//! inherited `PASTURE_*` — and assert on exit code and output, which is exactly
//! what a new user sees.
//!
//! They are deliberately hermetic: nothing here contacts a network service, and
//! the Ollama port is pointed at a closed one so a developer who happens to be
//! running Ollama gets the same result as CI.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pasture");

/// A throwaway `HOME` so the config file / cost log land somewhere harmless.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pasture-cli-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

/// Run the binary with an empty environment: no `PATH` (so nothing is
/// installed), no inherited `PASTURE_*`, and English messages.
fn run(tag: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let home = scratch_home(tag);
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env_clear()
        .env("PATH", "")
        .env("HOME", &home)
        // Nothing listens on port 1; this keeps a developer's running Ollama
        // from changing the outcome.
        .env("PASTURE_OLLAMA_PORT", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run pasture")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// ADR-267 (A9): with no `ollama` on PATH, `up` must name *that* — and must not
/// spawn nothing and then poll a dead port for three seconds first.
#[test]
fn up_without_ollama_on_path_fails_fast_and_says_why() {
    let started = std::time::Instant::now();
    let out = run("up-nopath", &["up"], &[]);
    let elapsed = started.elapsed();

    assert_eq!(out.status.code(), Some(1), "up must fail: {}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("was not found on PATH"),
        "must name the real cause, got: {err}"
    );
    // The old code slept 15 x 200ms before giving up. Allow generous slack for a
    // loaded CI box while still failing if the poll loop comes back.
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "up should not poll for an Ollama it never spawned (took {elapsed:?})"
    );
}

/// ADR-265 (A1/A2): `doctor` must offer the *install* fix (not the *start* fix)
/// when the command is absent, and must still report on the model check rather
/// than skipping it because the server was unreachable.
#[test]
fn doctor_on_a_bare_machine_reports_both_problems_with_the_right_fix() {
    let out = run("doctor-bare", &["doctor"], &[]);
    let text = stdout(&out) + &stderr(&out);

    assert!(
        text.contains("Ollama is not installed"),
        "wrong fix for an absent command: {text}"
    );
    assert!(
        !text.contains("Ollama is installed but not running"),
        "must not offer both fixes at once: {text}"
    );
    // A2: the model step used to sit behind `if reachable`, so it printed
    // nothing at all on the machine that most needed it.
    assert!(
        text.to_lowercase().contains("model"),
        "the model check must still report: {text}"
    );
}

/// ADR-265 (A3): an unwritable cost log is not "no data yet". `stats` used to
/// say "run some requests first" forever; it must name the cause and exit 1.
#[test]
fn stats_distinguishes_an_unwritable_log_from_an_empty_one() {
    // A path whose parent cannot be created.
    let bad = "/proc/pasture-does-not-exist/cost.jsonl";
    let out = run("stats-bad", &["stats"], &[("PASTURE_COST_LOG", bad)]);
    let text = stdout(&out) + &stderr(&out);

    assert_eq!(out.status.code(), Some(1), "must fail loudly: {text}");
    assert!(
        !text.contains("run some requests"),
        "an unwritable log must not be reported as 'no data yet': {text}"
    );
}

/// ADR-266 (A4): a malformed value and a misspelled name both used to vanish in
/// silence, leaving the user with defaults they never chose.
#[test]
fn bad_env_settings_are_reported_instead_of_silently_dropped() {
    let out = run(
        "env-bad",
        &["version"],
        &[
            ("PASTURE_THRESHOLD", "notanumber"),
            ("PASTURE_LOCAL_BAKEND", "lmstudio"),
        ],
    );
    let err = stderr(&out);
    assert!(
        err.contains("PASTURE_THRESHOLD") && err.contains("not a number"),
        "malformed value must be reported: {err}"
    );
    assert!(
        err.contains("PASTURE_LOCAL_BAKEND"),
        "unknown setting must be reported: {err}"
    );
}

/// A clean environment must stay quiet — a warning channel that always fires is
/// one nobody reads.
#[test]
fn a_clean_environment_produces_no_warnings() {
    let out = run("env-clean", &["version"], &[]);
    assert_eq!(stderr(&out), "", "unexpected warnings on a clean run");
}

/// ADR-261 (A7): the hardware tier must actually reach routing. This is the
/// product's only real differentiator and it was inverted off Linux; pin both
/// sides of the RAM boundary end-to-end through the binary.
#[test]
fn ram_tier_reaches_the_routing_threshold() {
    let long = "word ".repeat(500);
    for (ram, threshold) in [("4000", "300"), ("64000", "800")] {
        let out = run(
            "route-ram",
            &["route", &long, "--json"],
            &[("PASTURE_RAM_MB", ram)],
        );
        let text = stdout(&out);
        assert!(
            text.contains(&format!("threshold {threshold}")),
            "RAM {ram} MB should route against threshold {threshold}, got: {text}"
        );
    }
}

/// ADR-261 (A7): "undetectable" must render as unknown, never as `0 MB` — the
/// value that used to be read as "tiniest possible machine".
#[test]
fn hw_never_reports_zero_mb_of_ram() {
    let out = run("hw", &["hw"], &[]);
    let text = stdout(&out);
    assert!(
        !text.contains("0 MB"),
        "0 MB is the bug ADR-261 fixed, not a hardware reading: {text}"
    );
}

/// A one-shot std-only HTTP stub speaking just enough Ollama to drive `doctor`:
/// `/api/tags` (reachability + model list) and `/api/version` (ADR-274). No
/// python, no crates — it is a thread and a `TcpListener`.
fn fake_ollama(version: &str, model: &str) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let version = version.to_string();
    let model = model.to_string();
    let handle = std::thread::spawn(move || {
        // doctor makes two GETs per run; serve a bounded number then exit so
        // the thread can never outlive the test.
        for _ in 0..8 {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if req.contains("/api/version") {
                format!("{{\"version\":\"{version}\"}}")
            } else {
                format!("{{\"models\":[{{\"name\":\"{model}\"}}]}}")
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    (port, handle)
}

/// The problem count and the `[!!]` markers must agree (ADR-274).
///
/// `doctor` ends with "N item(s) need attention (see [!!] above)". The catalog
/// splits markers by meaning — `[!!]` is a counted defect, `[--]` is
/// information that is never counted. ADR-274 initially incremented the
/// counter while printing a `[--]` line, so `doctor` reported one item needing
/// attention with no `[!!]` on screen at all.
///
/// Asserted on the real binary's real output, because that is where the
/// invariant lives: a source-scan version of this test passed the very
/// mutation it existed to catch, since a neighbouring `[!!]` message sat
/// inside its window.
#[test]
fn doctor_problem_count_matches_the_bang_markers() {
    let cases = [
        // (ollama version, cascade on) — the ADR-274 branch both ways, plus
        // the bare machine, which exercises the pre-existing counted lines.
        ("0.12.11", false),
        ("0.12.10", false),
        ("0.12.10", true),
    ];
    for (version, cascade) in cases {
        let (port, handle) = fake_ollama(version, "llama3.2:latest");
        let p = port.to_string();
        let mut env: Vec<(&str, &str)> = vec![
            ("PASTURE_OLLAMA_PORT", &p),
            ("PASTURE_LOCAL_MODEL", "llama3.2"),
        ];
        if cascade {
            env.push(("PASTURE_CASCADE", "1"));
        }
        let out = run("doctor-markers", &["doctor"], &env);
        let text = stdout(&out);
        // The summary line itself reads "(see [!!] above)", so exclude it or
        // it counts as one of the very markers it is describing.
        let bangs = text
            .lines()
            .filter(|l| l.contains("[!!]") && !l.contains("need attention"))
            .count();
        let claimed: usize = text
            .lines()
            .find_map(|l| {
                l.split_whitespace()
                    .next()
                    .and_then(|w| w.parse().ok())
                    .filter(|_| l.contains("need attention"))
            })
            .unwrap_or(0);
        assert_eq!(
            claimed, bangs,
            "ollama {version}, cascade={cascade}: doctor claims {claimed} item(s) need \
attention but printed {bangs} [!!] line(s).\n{text}"
        );
        drop(handle);
    }
}

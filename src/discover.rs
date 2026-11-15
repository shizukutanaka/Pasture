//! Local LLM runtime discovery (IMP-75, ported from lmm's detection
//! know-how in the local-LLM consolidation).
//!
//! Pasture routes to a local backend, but a new user's first problem is
//! more basic: *what is already running on this machine?* lmm's one
//! genuinely broader feature was probing for the many local runtimes
//! people actually install. This module carries that list: each known
//! runtime's default port and a cheap HTTP path to knock on. A probe only
//! counts as "found" when something answers with an HTTP response — an
//! open port alone proves nothing (it could be any other program).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// One known local runtime: where to knock and what to tell the user
/// when it answers.
pub struct RuntimeProbe {
    pub name: &'static str,
    pub port: u16,
    /// A cheap, side-effect-free GET path the runtime serves.
    pub path: &'static str,
    /// How to use this runtime with Pasture, shown when it is found.
    pub hint: &'static str,
}

/// The known local runtimes and their default ports. Pure data — the
/// unit tests pin its integrity (unique ports, absolute paths).
pub fn known_runtimes() -> Vec<RuntimeProbe> {
    vec![
        RuntimeProbe {
            name: "Ollama",
            port: 11434,
            path: "/api/tags",
            hint: "Pasture's default local backend — nothing to configure.",
        },
        RuntimeProbe {
            name: "LM Studio",
            port: 1234,
            path: "/v1/models",
            hint: "Use it via PASTURE_LOCAL_BACKEND=lmstudio (or PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:1234/v1). Start its server first (lms server start).",
        },
        RuntimeProbe {
            name: "Jan",
            port: 1337,
            path: "/v1/models",
            hint: "Use it via PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:1337/v1 (Jan's local API server must be enabled).",
        },
        RuntimeProbe {
            name: "llama.cpp server",
            port: 8080,
            path: "/health",
            hint: "Use it via PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:8080/v1.",
        },
        RuntimeProbe {
            name: "KoboldCPP",
            port: 5001,
            path: "/api/v1/model",
            hint: "Use it via PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:5001/v1.",
        },
        RuntimeProbe {
            name: "GPT4All",
            port: 4891,
            path: "/v1/models",
            hint: "Use it via PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:4891/v1 (GPT4All's server mode must be enabled).",
        },
        RuntimeProbe {
            name: "vLLM",
            port: 8000,
            path: "/v1/models",
            hint: "Use it via PASTURE_LOCAL_OPENAI_URL=http://127.0.0.1:8000/v1.",
        },
        RuntimeProbe {
            name: "Open WebUI",
            port: 3000,
            path: "/api/config",
            hint: "A chat frontend, not a backend: point Open WebUI at Pasture (an OpenAI-compatible connection) so its traffic gets routed.",
        },
    ]
}

/// Probe one runtime on `host`: connect, send a minimal HTTP GET, and
/// count it as found only when the reply starts with an HTTP status line.
pub fn probe_one(host: &str, probe: &RuntimeProbe, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addrs) = format!("{host}:{}", probe.port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!(
        "GET {} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        probe.path
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) if n >= 5 => &buf[..5] == b"HTTP/",
        _ => false,
    }
}

/// Probe every known runtime on `host`, returning each with its verdict.
pub fn run_discovery(host: &str, timeout: Duration) -> Vec<(RuntimeProbe, bool)> {
    known_runtimes()
        .into_iter()
        .map(|p| {
            let found = probe_one(host, &p, timeout);
            (p, found)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn test_known_runtimes_are_wellformed() {
        let rt = known_runtimes();
        assert!(rt.len() >= 6, "the list should cover the common runtimes");
        let mut ports: Vec<u16> = rt.iter().map(|p| p.port).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), rt.len(), "ports must be unique");
        for p in &rt {
            assert!(!p.name.is_empty());
            assert!(p.path.starts_with('/'), "probe path must be absolute");
            assert!(!p.hint.is_empty());
        }
        assert!(
            rt.iter().any(|p| p.name == "Ollama" && p.port == 11434),
            "Ollama is Pasture's default backend and must be listed"
        );
    }

    #[test]
    fn test_probe_detects_an_http_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
        });
        let probe = RuntimeProbe {
            name: "test",
            port,
            path: "/x",
            hint: "h",
        };
        assert!(probe_one("127.0.0.1", &probe, Duration::from_millis(500)));
        server.join().unwrap();
    }

    #[test]
    fn test_probe_rejects_a_non_http_listener() {
        // Something is listening but never speaks HTTP → not a runtime.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (_s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let probe = RuntimeProbe {
            name: "test",
            port,
            path: "/x",
            hint: "h",
        };
        assert!(!probe_one("127.0.0.1", &probe, Duration::from_millis(100)));
        server.join().unwrap();
    }

    #[test]
    fn test_probe_closed_port_is_not_found() {
        // Bind then drop to get a port that is (almost certainly) closed.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let probe = RuntimeProbe {
            name: "test",
            port,
            path: "/x",
            hint: "h",
        };
        assert!(!probe_one("127.0.0.1", &probe, Duration::from_millis(200)));
    }
}

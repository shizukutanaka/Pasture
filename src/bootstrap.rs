//! Engine bootstrap (IMP-78, ADR-287): install the local inference
//! engine Pasture routes to, so a first-time user does not have to
//! fetch one by hand before `pasture up` can do anything.
//!
//! `pasture engine` is, besides `pasture update`, the only command
//! that talks to the network, and only when the user runs it: no
//! background check, no scheduled phone-home, nothing about the user
//! is sent — the requests are anonymous GETs for the public release
//! metadata and the archive itself.
//!
//! The engine is Ollama, Pasture's default local backend, taken from
//! its official GitHub releases (github.com/ollama/ollama). Trust
//! model, mirroring `update`: a downloaded archive is installed only
//! after its SHA-256 matches the checksum published with the release —
//! the `sha256sum.txt` asset Ollama attaches to every release when
//! present, otherwise the per-asset `digest` the GitHub API reports;
//! if both exist and disagree, or neither exists, the install refuses.
//! The extracted binary must also run and report the release's version
//! before it counts as installed, and it is then started and probed:
//! "installed" is only claimed once the engine actually answers.
//!
//! The install is user-local (under `PASTURE_ENGINE_HOME`, default
//! `~/.pasture/engines`): no administrator rights, no system paths, and
//! a failed attempt removes its staging area and leaves any existing
//! installation untouched.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Where engine release metadata comes from when
/// `PASTURE_ENGINE_API_BASE` is unset: Ollama's official GitHub
/// Releases API.
pub const DEFAULT_ENGINE_API_BASE: &str = "https://api.github.com/repos/ollama/ollama";

/// Checksums asset names, in preference order: Ollama attaches
/// `sha256sum.txt` to every release; `SHA256SUMS` is accepted too so a
/// mirror laid out like Pasture's own releases still verifies.
const SUMS_ASSET_NAMES: [&str; 2] = ["sha256sum.txt", "SHA256SUMS"];

/// Cap on an engine archive download. The largest asset Pasture will
/// fetch (Ollama's Linux bundles with GPU runtimes) is ~1.7 GB; the
/// cap exists so a broken or hostile endpoint cannot fill the disk.
const MAX_ENGINE_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Base URL of the engine releases API (no trailing slash).
/// Overridable for mirrors and for tests, which point it at a local
/// fake server — the same arrangement as `PASTURE_UPDATE_API_BASE`.
pub fn engine_api_base() -> String {
    match std::env::var("PASTURE_ENGINE_API_BASE") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => DEFAULT_ENGINE_API_BASE.to_string(),
    }
}

/// Root of Pasture's managed engine installs. `PASTURE_ENGINE_HOME`
/// overrides it (an engine bundle is gigabytes, so putting it on a
/// bigger disk is a legitimate need); the default is
/// `<home>/.pasture/engines`, with the same HOME/USERPROFILE fallback
/// chain as the config file (ADR-264).
pub fn engine_home() -> PathBuf {
    if let Ok(v) = std::env::var("PASTURE_ENGINE_HOME") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    home.join(".pasture").join("engines")
}

/// True for hosts that name this machine. Bootstrap installs and
/// starts a *local* engine; a configuration pointing at a remote
/// Ollama is the user's deliberate choice and is left alone.
pub fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let h = host.trim_start_matches('[').trim_end_matches(']');
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Map an (OS, arch) pair to Ollama's official release-asset name for
/// it. None for anything Ollama does not publish (32-bit, BSDs, ...):
/// those users install Ollama by hand, and the bootstrap says so
/// instead of guessing an asset name. The macOS asset is a single
/// universal build, so both architectures map to it.
pub fn platform_engine_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("ollama-linux-amd64.tar.zst"),
        ("linux", "aarch64") => Some("ollama-linux-arm64.tar.zst"),
        ("macos", "x86_64") | ("macos", "aarch64") => Some("ollama-darwin.tgz"),
        ("windows", "x86_64") => Some("ollama-windows-amd64.zip"),
        ("windows", "aarch64") => Some("ollama-windows-arm64.zip"),
        _ => None,
    }
}

/// Format a byte count for the install plan: an approximate human
/// size plus the exact figure, so the user can judge the download
/// either way.
pub fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB ({bytes} bytes)", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB ({bytes} bytes)", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB ({bytes} bytes)", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

// ---------------------------------------------------------------------
// Release plan: what would be (or is being) installed, resolved before
// any large download happens so the user sees source, size and
// verification method first.
// ---------------------------------------------------------------------

/// Everything about the engine release selected for this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct EnginePlan {
    /// Release tag, e.g. "v0.40.16".
    pub tag: String,
    pub version: (u64, u64, u64),
    pub asset_name: String,
    pub download_url: String,
    /// Asset size from the release metadata, when the API reports it.
    pub size_bytes: Option<u64>,
    /// The SHA-256 the downloaded archive must match, resolved with
    /// `update`'s policy (published sums win, API digest is the
    /// fallback, disagreement or absence refuses).
    pub expected_sha256: String,
}

/// Fetch the latest engine release's metadata and select this
/// platform's asset, resolving the checksum the download must match.
/// Nothing is downloaded at full size here — only the metadata and
/// the small checksums file.
pub fn fetch_engine_plan(api_base: &str, os: &str, arch: &str) -> Result<EnginePlan, String> {
    let asset_name = platform_engine_asset(os, arch).ok_or_else(|| {
        format!(
            "no official Ollama release asset exists for {os}-{arch}; install Ollama \
             manually from https://ollama.com, then run `pasture up`"
        )
    })?;
    let meta = crate::update::fetch(&format!("{api_base}/releases/latest"))?;
    let body = String::from_utf8_lossy(&meta).into_owned();
    let release = crate::update::parse_release(&body)?;
    let asset = release
        .asset(asset_name)
        .ok_or_else(|| {
            format!(
                "the {} release has no asset for this platform ({asset_name})",
                release.tag
            )
        })?
        .clone();
    let sums_text = fetch_sums(&release)?;
    let expected_sha256 =
        crate::update::expected_sha256(&release, asset_name, sums_text.as_deref())?;
    Ok(EnginePlan {
        tag: release.tag.clone(),
        version: release.version,
        asset_name: asset_name.to_string(),
        download_url: asset.download_url,
        size_bytes: asset_size(&body, asset_name),
        expected_sha256,
    })
}

/// Download the release's checksums file, if it attaches one. Its
/// absence is not fatal here — `update::expected_sha256` decides
/// whether the remaining evidence (the API digest) is enough.
fn fetch_sums(release: &crate::update::ReleaseInfo) -> Result<Option<String>, String> {
    for name in SUMS_ASSET_NAMES {
        if let Some(asset) = release.asset(name) {
            let bytes = crate::update::fetch(&asset.download_url)?;
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    Ok(None)
}

/// The `size` the API reports for one asset, read from the same
/// release JSON (`update::ReleaseAsset` does not carry it: the
/// updater never needs it, the install plan does).
fn asset_size(release_json: &str, asset_name: &str) -> Option<u64> {
    let root = crate::json::parse(release_json).ok()?;
    let assets = root.get("assets")?.as_array()?;
    for item in assets {
        if item.get("name").and_then(|v| v.as_str()) == Some(asset_name) {
            return item
                .get("size")
                .and_then(|v| v.as_f64())
                .filter(|s| *s >= 0.0)
                .map(|s| s as u64);
        }
    }
    None
}

// ---------------------------------------------------------------------
// Streaming download. `update::fetch` buffers the whole body, which
// release archives (~0.5 MB) forgive and engine archives (~1.7 GB)
// do not, so the bootstrap streams to disk, hashing as it goes.
// ---------------------------------------------------------------------

/// A connected request/response stream: plain TCP for http, TLS
/// (cloud feature) for https.
trait IoStream: Read + Write {}
impl IoStream for std::net::TcpStream {}
#[cfg(feature = "cloud")]
impl IoStream for native_tls::TlsStream<std::net::TcpStream> {}

fn tcp_connect(host: &str, port: u16) -> Result<std::net::TcpStream, String> {
    let stream = std::net::TcpStream::connect((host, port))
        .map_err(|e| format!("connect to {host}:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    Ok(stream)
}

#[cfg(feature = "cloud")]
fn connect_stream(scheme: &str, host: &str, port: u16) -> Result<Box<dyn IoStream>, String> {
    match scheme {
        "http" => Ok(Box::new(tcp_connect(host, port)?)),
        "https" => {
            let connector =
                native_tls::TlsConnector::new().map_err(|e| format!("tls init: {e}"))?;
            let tcp = tcp_connect(host, port)?;
            let stream = connector
                .connect(host, tcp)
                .map_err(|e| format!("tls handshake with {host}: {e}"))?;
            Ok(Box::new(stream))
        }
        other => Err(format!("unsupported URL scheme: {other}")),
    }
}

#[cfg(not(feature = "cloud"))]
fn connect_stream(scheme: &str, host: &str, port: u16) -> Result<Box<dyn IoStream>, String> {
    match scheme {
        "http" => Ok(Box::new(tcp_connect(host, port)?)),
        "https" => Err(format!(
            "this build has no TLS support, so it cannot reach {host}; engine install \
             over HTTPS is available in the prebuilt release binaries"
        )),
        other => Err(format!("unsupported URL scheme: {other}")),
    }
}

/// A buffered reader over the response stream. Header parsing and
/// chunked decoding consume the front; body bytes already read past
/// the header are served first, so nothing is lost at the seam.
struct StreamReader {
    stream: Box<dyn IoStream>,
    buf: Vec<u8>,
    start: usize,
    eof: bool,
}

impl StreamReader {
    fn new(stream: Box<dyn IoStream>) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            start: 0,
            eof: false,
        }
    }

    fn fill(&mut self) -> Result<(), String> {
        if self.eof {
            return Ok(());
        }
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let mut chunk = [0u8; 16 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => {
                self.eof = true;
                Ok(())
            }
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e) => Err(format!("read from server: {e}")),
        }
    }

    fn avail(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Read one response head (status line + headers), leaving any
    /// body bytes buffered for the body phase.
    fn read_head(&mut self) -> Result<Vec<u8>, String> {
        loop {
            if let Some(idx) = crate::http::find_subslice(&self.buf[self.start..], b"\r\n\r\n") {
                let head = self.buf[self.start..self.start + idx].to_vec();
                self.start += idx + 4;
                if self.start == self.buf.len() {
                    self.buf.clear();
                    self.start = 0;
                }
                return Ok(head);
            }
            if self.avail() > 64 * 1024 {
                return Err("HTTP response header is implausibly large".to_string());
            }
            if self.eof {
                return Err("server closed the connection mid-header".to_string());
            }
            self.fill()?;
        }
    }

    /// Read one CRLF (or bare LF) terminated line — chunk sizes and
    /// trailers. The returned line excludes the terminator.
    fn read_line(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            if self.avail() == 0 {
                if self.eof {
                    return Ok(out);
                }
                self.fill()?;
                continue;
            }
            let byte = self.buf[self.start];
            self.start += 1;
            if self.start == self.buf.len() {
                self.buf.clear();
                self.start = 0;
            }
            if byte == b'\n' {
                if out.last() == Some(&b'\r') {
                    out.pop();
                }
                return Ok(out);
            }
            out.push(byte);
            if out.len() > 8192 {
                return Err("an HTTP line is implausibly long".to_string());
            }
        }
    }

    /// Stream exactly `n` body bytes into the sink.
    fn pump_n(&mut self, mut n: u64, sink: &mut DownloadSink<'_>) -> Result<(), String> {
        while n > 0 {
            if self.avail() == 0 {
                if self.eof {
                    return Err("download ended early (connection closed)".to_string());
                }
                self.fill()?;
                continue;
            }
            let take = (n as usize).min(self.avail());
            let (start, end) = (self.start, self.start + take);
            sink.push(&self.buf[start..end])?;
            self.start = end;
            if self.start == self.buf.len() {
                self.buf.clear();
                self.start = 0;
            }
            n -= take as u64;
        }
        Ok(())
    }

    /// Stream body bytes until the server closes the connection.
    fn pump_to_eof(&mut self, sink: &mut DownloadSink<'_>) -> Result<(), String> {
        loop {
            if self.avail() > 0 {
                let (start, end) = (self.start, self.buf.len());
                sink.push(&self.buf[start..end])?;
                self.buf.clear();
                self.start = 0;
            }
            if self.eof {
                return Ok(());
            }
            self.fill()?;
        }
    }
}

/// Where a streaming download goes: the archive file on disk, the
/// running SHA-256, the byte counter, the size cap and the progress
/// callback, bundled because every body path needs all of them.
struct DownloadSink<'a> {
    writer: std::io::BufWriter<std::fs::File>,
    hasher: crate::update::Sha256,
    total: u64,
    max_bytes: u64,
    expected_total: Option<u64>,
    progress: &'a mut (dyn FnMut(u64, Option<u64>) + 'a),
    last_report: u64,
}

impl DownloadSink<'_> {
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.total = self.total.saturating_add(bytes.len() as u64);
        if self.total > self.max_bytes {
            return Err(format!(
                "download exceeds the {}-byte cap; aborting",
                self.max_bytes
            ));
        }
        self.writer
            .write_all(bytes)
            .map_err(|e| format!("cannot write the download: {e}"))?;
        self.hasher.update(bytes);
        if self.total - self.last_report >= 32 * 1024 * 1024 {
            self.last_report = self.total;
            (self.progress)(self.total, self.expected_total);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(u64, String), String> {
        self.writer
            .flush()
            .map_err(|e| format!("cannot write the download: {e}"))?;
        (self.progress)(self.total, self.expected_total);
        Ok((self.total, self.hasher.finalize_hex()))
    }
}

enum DownloadOnce {
    Done(u64, String),
    Redirect(String),
}

/// GET `url` streamed into `dest`, hashing while downloading.
/// Returns (bytes written, lowercase SHA-256 hex). Follows redirects
/// like `update::fetch`. On any error the partial file is removed:
/// a half-downloaded engine is never left behind.
pub fn download_to_file(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<(u64, String), String> {
    let result = download_follow(url, dest, max_bytes, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn download_follow(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<(u64, String), String> {
    let mut current = url.to_string();
    for _ in 0..6 {
        match download_once(&current, dest, max_bytes, progress)? {
            DownloadOnce::Done(n, sha) => return Ok((n, sha)),
            DownloadOnce::Redirect(location) => {
                current = crate::update::resolve_redirect(&current, &location);
            }
        }
    }
    Err("too many redirects".to_string())
}

fn download_once(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<DownloadOnce, String> {
    let (scheme, host, port, path) = crate::update::parse_url(url)?;
    let host_header = if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: pasture/{} (pasture engine)\r\nAccept: application/octet-stream\r\nConnection: close\r\n\r\n",
        crate::VERSION
    );
    let mut stream = connect_stream(&scheme, &host, port)?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("send to {host}: {e}"))?;
    let mut reader = StreamReader::new(stream);
    // Interim 1xx heads precede the real one.
    let (status, headers) = loop {
        let head = reader.read_head()?;
        let parsed = parse_head(&head)?;
        if (100..200).contains(&parsed.0) {
            continue;
        }
        break parsed;
    };
    match status {
        301 | 302 | 303 | 307 | 308 => {
            let location = header_value(&headers, "location")
                .ok_or("the server redirected without a Location header")?;
            return Ok(DownloadOnce::Redirect(location.to_string()));
        }
        200..=299 => {}
        other => return Err(format!("GET {url} failed: HTTP {other}")),
    }
    let content_length =
        header_value(&headers, "content-length").and_then(|v| v.parse::<u64>().ok());
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let file = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut sink = DownloadSink {
        writer: std::io::BufWriter::new(file),
        hasher: crate::update::Sha256::new(),
        total: 0,
        max_bytes,
        expected_total: content_length,
        progress,
        last_report: 0,
    };
    if chunked {
        loop {
            let line = reader.read_line()?;
            let text = String::from_utf8_lossy(&line).into_owned();
            let size =
                usize::from_str_radix(text.trim().split(';').next().unwrap_or("0").trim(), 16)
                    .map_err(|_| "chunked body: bad chunk size".to_string())?;
            if size == 0 {
                // Trailers end at an empty line; EOF is also fine.
                loop {
                    match reader.read_line() {
                        Ok(line) if !line.is_empty() => continue,
                        _ => break,
                    }
                }
                break;
            }
            reader.pump_n(size as u64, &mut sink)?;
            let crlf = reader.read_line()?;
            if !crlf.is_empty() {
                return Err("chunked body: missing chunk terminator".to_string());
            }
        }
    } else if let Some(n) = content_length {
        reader.pump_n(n, &mut sink)?;
    } else {
        reader.pump_to_eof(&mut sink)?;
    }
    let (bytes, sha256) = sink.finish()?;
    Ok(DownloadOnce::Done(bytes, sha256))
}

/// Parse a response head (the part before the blank line) into a
/// status code and lowercased header pairs.
fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), String> {
    let text = String::from_utf8_lossy(head).into_owned();
    let mut lines = text.lines();
    let status_line = lines.next().ok_or("empty HTTP response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed HTTP status line: {status_line}"))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------
// Locating engine binaries: on PATH (the user's own install) or in
// Pasture's managed tree.
// ---------------------------------------------------------------------

/// Find an executable named `name` on PATH and return its full path
/// (doctor::on_path answers the same question as a bool; the
/// bootstrap needs the path so it can start what it found).
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let candidates = [name.to_string(), format!("{name}.exe")];
    for dir in std::env::split_paths(&path) {
        for c in &candidates {
            let p = dir.join(c);
            if std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
                return Some(p);
            }
        }
    }
    None
}

/// Locate the engine binary inside an extracted or installed tree
/// (layouts differ per platform: `bin/ollama` + `lib/...` on Linux,
/// the binary at the root on macOS/Windows). Deterministic: the
/// shallowest match wins, ties break on the path.
pub fn find_engine_binary(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    collect_engine_binaries(dir, 0, &mut found);
    found.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    found.into_iter().next()
}

fn collect_engine_binaries(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "ollama" || name == "ollama.exe" {
                out.push(path);
            }
        }
    }
    for sub in subdirs {
        collect_engine_binaries(&sub, depth + 1, out);
    }
}

/// The newest managed Ollama install's binary, if any. Managed
/// installs live at `<engine_home>/ollama-<tag>/`; a directory only
/// counts when a binary is actually inside it.
pub fn managed_ollama_binary(engine_home: &Path) -> Option<PathBuf> {
    let mut installs: Vec<((u64, u64, u64), PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(engine_home).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(tag) = name.strip_prefix("ollama-") else {
            continue;
        };
        let Some(version) = crate::update::parse_version(tag) else {
            continue;
        };
        if let Some(binary) = find_engine_binary(&path) {
            installs.push((version, binary));
        }
    }
    installs.sort();
    installs.pop().map(|(_, binary)| binary)
}

/// The engine binary to use, wherever it lives: the user's own
/// install on PATH first (never shadow a deliberate install), then
/// Pasture's managed tree.
pub fn resolve_ollama_binary(engine_home: &Path) -> Option<PathBuf> {
    which_on_path("ollama").or_else(|| managed_ollama_binary(engine_home))
}

/// Run `<path> --version` and parse the version Ollama reports
/// ("ollama version is 0.40.16"). The output is scanned token by
/// token because Ollama prefixes some builds with a warning line;
/// the first token that parses as a version wins.
pub fn ollama_binary_version(path: &Path) -> Option<(u64, u64, u64)> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().find_map(|tok| {
        crate::update::parse_version(
            tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'),
        )
    })
}

/// Why the engine archive could not be unpacked on this machine, if
/// it couldn't (pure, so the matrix is unit-testable). GNU tar unpacks
/// .tar.zst through the `zstd` program — it does not carry the codec
/// itself (measured: this project's build sandbox has libzstd present
/// but its GNU tar does not link it) — while bsdtar's libarchive has
/// zstd built in. A missing zstd must surface before the download,
/// not after 1.7 GB of it.
fn extract_blocker(
    asset_name: &str,
    tar_version: Option<&str>,
    zstd_on_path: bool,
) -> Option<String> {
    let tar_version = match tar_version {
        Some(v) => v,
        None => {
            return Some(
                "the `tar` program is required to unpack the engine but was not found \
                 on PATH; install tar, or install Ollama manually from https://ollama.com"
                    .to_string(),
            )
        }
    };
    if asset_name.ends_with(".tar.zst") && !tar_version.contains("bsdtar") && !zstd_on_path {
        return Some(
            "unpacking the official Linux engine archive (.tar.zst) needs the `zstd` \
             program, which was not found on PATH; install zstd (e.g. `sudo apt install \
             zstd` / `brew install zstd`), or install Ollama manually from \
             https://ollama.com"
                .to_string(),
        );
    }
    None
}

/// Fail fast — before a multi-gigabyte download — when the archive
/// could not be extracted on this machine once it arrived.
fn preflight_extract(asset_name: &str) -> Result<(), String> {
    let tar_version = std::process::Command::new("tar")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    match extract_blocker(
        asset_name,
        tar_version.as_deref(),
        crate::doctor::on_path("zstd"),
    ) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------
// Orchestration.
// ---------------------------------------------------------------------

/// Inputs to a bootstrap run, gathered from the real environment by
/// `run_engine` / `pasture up` and fabricated by tests.
pub struct BootstrapPlan {
    pub api_base: String,
    pub engine_home: PathBuf,
    pub os: String,
    pub arch: String,
    pub ollama_host: String,
    pub ollama_port: u16,
    pub check_only: bool,
    pub lang: crate::i18n::Lang,
}

impl BootstrapPlan {
    /// The plan for the current environment and the configured
    /// Ollama endpoint.
    pub fn from_env(ollama_host: &str, ollama_port: u16) -> Self {
        Self {
            api_base: engine_api_base(),
            engine_home: engine_home(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            ollama_host: ollama_host.to_string(),
            ollama_port,
            check_only: false,
            lang: crate::i18n::detect(),
        }
    }
}

/// A located engine binary and how it got there.
#[derive(Debug)]
pub struct EngineBinary {
    pub path: PathBuf,
    /// True when this run downloaded and installed it.
    pub freshly_installed: bool,
    /// The version the binary reports, when it answered `--version`.
    pub version: Option<(u64, u64, u64)>,
}

#[derive(Debug)]
pub enum BootstrapOutcome {
    /// A runtime is already answering on this machine; nothing to do.
    AlreadyRunning { name: String, hint: String },
    /// `--check`: nothing was running or installed; this is what a
    /// real run would install.
    WouldInstall { plan: EnginePlan },
    /// `--check`: a binary exists but no engine is running. Starting
    /// it is a side effect, so a check-only run reports instead.
    InstalledNotRunning {
        binary: PathBuf,
        version: Option<(u64, u64, u64)>,
    },
    /// The engine is installed and answering on the Ollama endpoint.
    Started {
        binary: PathBuf,
        version: String,
        endpoint: String,
        freshly_installed: bool,
    },
    /// The binary is installed and verified, but it did not start
    /// answering in time. Not a silent failure: the user gets the
    /// binary path, the log path and the reason.
    InstalledNotStarted {
        binary: PathBuf,
        version: String,
        log_path: PathBuf,
        detail: String,
    },
}

/// Ensure an Ollama binary exists on this machine, installing it
/// (with the plan announced through `log` before the download starts)
/// when neither PATH nor the managed tree has one.
pub fn ensure_ollama_binary(
    plan: &BootstrapPlan,
    log: &mut (dyn FnMut(&str) + '_),
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<EngineBinary, String> {
    if let Some(path) = resolve_ollama_binary(&plan.engine_home) {
        let version = ollama_binary_version(&path);
        return Ok(EngineBinary {
            path,
            freshly_installed: false,
            version,
        });
    }
    let engine_plan = fetch_engine_plan(&plan.api_base, &plan.os, &plan.arch)?;
    install_engine(plan, &engine_plan, log, progress)
}

/// Download, verify and place one engine release. The binary is only
/// moved into the managed tree after the archive's SHA-256 matched
/// and the extracted binary reported the release's version; any
/// failure removes the staging area and installs nothing.
fn install_engine(
    plan: &BootstrapPlan,
    engine_plan: &EnginePlan,
    log: &mut (dyn FnMut(&str) + '_),
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<EngineBinary, String> {
    use crate::i18n::tf;
    preflight_extract(&engine_plan.asset_name)?;
    let size = engine_plan
        .size_bytes
        .map(human_size)
        .unwrap_or_else(|| "unknown size".to_string());
    log(&tf(
        plan.lang,
        "engine.plan",
        &[
            ("version", &engine_plan.tag),
            ("url", &engine_plan.download_url),
            ("size", &size),
        ],
    ));
    std::fs::create_dir_all(&plan.engine_home).map_err(|e| {
        format!(
            "cannot create the engine directory {}: {e}",
            plan.engine_home.display()
        )
    })?;
    // Unique per run (pid + timestamp), inside the engine home so the
    // final move is a rename within one filesystem.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging = plan
        .engine_home
        .join(format!(".staging-{}-{stamp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("cannot create a staging directory: {e}"))?;
    let result = install_into_staging(plan, engine_plan, &staging, progress);
    let _ = std::fs::remove_dir_all(&staging);
    let binary = result?;
    Ok(EngineBinary {
        path: binary,
        freshly_installed: true,
        version: Some(engine_plan.version),
    })
}

fn install_into_staging(
    plan: &BootstrapPlan,
    engine_plan: &EnginePlan,
    staging: &Path,
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<PathBuf, String> {
    let archive_path = staging.join(&engine_plan.asset_name);
    let (bytes, actual) = download_to_file(
        &engine_plan.download_url,
        &archive_path,
        MAX_ENGINE_DOWNLOAD_BYTES,
        progress,
    )?;
    if let Some(size) = engine_plan.size_bytes {
        if bytes != size {
            return Err(format!(
                "the download is incomplete ({bytes} of {size} bytes); it was discarded \
                 and nothing was installed"
            ));
        }
    }
    if !crate::http::constant_time_eq(actual.as_bytes(), engine_plan.expected_sha256.as_bytes()) {
        return Err(format!(
            "checksum mismatch for {} (expected {}, got {actual}); the download was \
             discarded and nothing was installed",
            engine_plan.asset_name, engine_plan.expected_sha256
        ));
    }
    let extract_dir = staging.join("extracted");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("cannot create a work directory: {e}"))?;
    crate::update::extract_archive(&archive_path, &extract_dir)?;
    let _ = std::fs::remove_file(&archive_path);
    let found = find_engine_binary(&extract_dir)
        .ok_or("the downloaded archive does not contain an ollama binary")?;
    // The binary must prove it is the release it claims to be before
    // it is allowed into the managed tree (the updater's rule, IMP-77).
    match ollama_binary_version(&found) {
        Some(v) if v == engine_plan.version => {}
        Some(v) => {
            return Err(format!(
                "the downloaded binary reports version {}, not {}; it was not installed",
                format_version(v),
                engine_plan.tag
            ))
        }
        None => {
            return Err("the downloaded binary did not run, so it was not installed".to_string())
        }
    }
    let final_dir = plan.engine_home.join(format!("ollama-{}", engine_plan.tag));
    if final_dir.exists() {
        // A previous attempt left a tree without a working binary
        // (a found one would have been resolved before downloading).
        std::fs::remove_dir_all(&final_dir)
            .map_err(|e| format!("cannot replace the previous engine install: {e}"))?;
    }
    std::fs::rename(&extract_dir, &final_dir)
        .map_err(|e| format!("cannot move the engine into {}: {e}", final_dir.display()))?;
    find_engine_binary(&final_dir)
        .ok_or_else(|| "the installed engine tree does not contain an ollama binary".to_string())
}

/// Start an engine binary and wait until it answers on the Ollama
/// endpoint. The server is detached (it must outlive this command),
/// with its output kept in a log file the user can inspect.
fn start_engine(plan: &BootstrapPlan, binary: &EngineBinary) -> BootstrapOutcome {
    let version = binary
        .version
        .map(format_version)
        .unwrap_or_else(|| "unknown".to_string());
    let endpoint = format!("http://{}:{}", plan.ollama_host, plan.ollama_port);
    let log_path = plan.engine_home.join("ollama-serve.log");
    let spawn = (|| -> Result<std::process::Child, String> {
        std::fs::create_dir_all(&plan.engine_home)
            .map_err(|e| format!("cannot create the engine directory: {e}"))?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("cannot open the engine log {}: {e}", log_path.display()))?;
        let log_err = log_file
            .try_clone()
            .map_err(|e| format!("cannot open the engine log: {e}"))?;
        std::process::Command::new(&binary.path)
            .arg("serve")
            .env(
                "OLLAMA_HOST",
                format!("{}:{}", plan.ollama_host, plan.ollama_port),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_err))
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", binary.path.display()))
    })();
    let mut child = match spawn {
        Ok(c) => c,
        Err(detail) => {
            return BootstrapOutcome::InstalledNotStarted {
                binary: binary.path.clone(),
                version,
                log_path,
                detail,
            }
        }
    };
    // 30 seconds, polled: a cold Ollama start is a few seconds even
    // on a slow disk; anything longer is a problem to report, not to
    // wait out silently.
    for _ in 0..120 {
        if crate::doctor::probe_ollama(&plan.ollama_host, plan.ollama_port).reachable {
            return BootstrapOutcome::Started {
                binary: binary.path.clone(),
                version,
                endpoint,
                freshly_installed: binary.freshly_installed,
            };
        }
        if let Ok(Some(status)) = child.try_wait() {
            return BootstrapOutcome::InstalledNotStarted {
                binary: binary.path.clone(),
                version,
                log_path,
                detail: format!("the engine exited immediately ({status})"),
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    BootstrapOutcome::InstalledNotStarted {
        binary: binary.path.clone(),
        version,
        log_path,
        detail: "the engine did not answer within 30 seconds".to_string(),
    }
}

/// The full bootstrap: detect a running engine, install Ollama if
/// there is none, start it, and prove it answers. Every failure
/// returns a user-readable reason.
pub fn perform_bootstrap(
    plan: &BootstrapPlan,
    log: &mut (dyn FnMut(&str) + '_),
    progress: &mut (dyn FnMut(u64, Option<u64>) + '_),
) -> Result<BootstrapOutcome, String> {
    if !is_loopback_host(&plan.ollama_host) {
        return Err(format!(
            "engine setup installs on this machine, but the configured Ollama host is {} \
             (not local); install an engine there, or unset PASTURE_OLLAMA_HOST",
            plan.ollama_host
        ));
    }
    // Anything already answering? Open WebUI is a frontend, not an
    // engine — it does not count (it needs an engine behind it).
    let found =
        crate::discover::run_discovery(&plan.ollama_host, std::time::Duration::from_millis(300))
            .into_iter()
            .find(|(probe, up)| *up && probe.name != "Open WebUI");
    if let Some((probe, _)) = found {
        return Ok(BootstrapOutcome::AlreadyRunning {
            name: probe.name.to_string(),
            hint: probe.hint.to_string(),
        });
    }
    if plan.check_only {
        return match resolve_ollama_binary(&plan.engine_home) {
            Some(path) => Ok(BootstrapOutcome::InstalledNotRunning {
                version: ollama_binary_version(&path),
                binary: path,
            }),
            None => {
                let engine_plan = fetch_engine_plan(&plan.api_base, &plan.os, &plan.arch)?;
                Ok(BootstrapOutcome::WouldInstall { plan: engine_plan })
            }
        };
    }
    let binary = ensure_ollama_binary(plan, log, progress)?;
    Ok(start_engine(plan, &binary))
}

/// Format a parsed version triple the way release tags write it.
pub fn format_version(v: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// A throttled download-progress printer for the CLI entry points:
/// one line per 64 MiB (and one at completion), so a 1.7 GB download
/// narrates itself without flooding the terminal.
pub fn progress_printer(lang: crate::i18n::Lang) -> impl FnMut(u64, Option<u64>) {
    let mut last = 0u64;
    move |done: u64, total: Option<u64>| {
        if done != total.unwrap_or(0) && done - last < 64 * 1024 * 1024 {
            return;
        }
        last = done;
        let done_s = human_size(done);
        match total {
            Some(t) => println!(
                "{}",
                crate::i18n::tf(
                    lang,
                    "engine.progress",
                    &[("done", &done_s), ("total", &human_size(t))]
                )
            ),
            None => println!(
                "{}",
                crate::i18n::tf(lang, "engine.progress.nototal", &[("done", &done_s)])
            ),
        }
    }
}

/// `pasture engine [--check]`: the CLI entry point. Returns an exit
/// code. Mirrors `update::run_update`'s shape.
pub fn run_engine(check_only: bool, ollama_host: &str, ollama_port: u16) -> i32 {
    use crate::i18n::{t, tf};
    let mut plan = BootstrapPlan::from_env(ollama_host, ollama_port);
    plan.check_only = check_only;
    let lang = plan.lang;
    // On a platform Ollama does not publish for, an install is
    // impossible — but an engine the user installed by hand can still
    // be started, so only refuse when there is no binary either.
    if platform_engine_asset(&plan.os, &plan.arch).is_none()
        && resolve_ollama_binary(&plan.engine_home).is_none()
    {
        println!(
            "{}",
            tf(
                lang,
                "engine.unsupported",
                &[("platform", &format!("{}-{}", plan.os, plan.arch))]
            )
        );
        return 1;
    }
    println!("{}", t(lang, "engine.checking"));
    let mut progress = progress_printer(lang);
    match perform_bootstrap(&plan, &mut |line| println!("{line}"), &mut progress) {
        Ok(BootstrapOutcome::AlreadyRunning { name, hint }) => {
            println!(
                "{}",
                tf(lang, "engine.found", &[("name", &name), ("hint", &hint)])
            );
            0
        }
        Ok(BootstrapOutcome::WouldInstall { plan: engine_plan }) => {
            let size = engine_plan
                .size_bytes
                .map(human_size)
                .unwrap_or_else(|| "unknown size".to_string());
            println!(
                "{}",
                tf(
                    lang,
                    "engine.wouldinstall",
                    &[
                        ("version", &engine_plan.tag),
                        ("size", &size),
                        ("url", &engine_plan.download_url),
                    ],
                )
            );
            0
        }
        Ok(BootstrapOutcome::InstalledNotRunning { binary, version }) => {
            let version = version
                .map(format_version)
                .unwrap_or_else(|| "unknown".to_string());
            println!(
                "{}",
                tf(
                    lang,
                    "engine.notrunning",
                    &[
                        ("version", &version),
                        ("path", &binary.display().to_string()),
                    ],
                )
            );
            0
        }
        Ok(BootstrapOutcome::Started {
            version, endpoint, ..
        }) => {
            println!(
                "{}",
                tf(
                    lang,
                    "engine.started",
                    &[("version", &version), ("endpoint", &endpoint)]
                )
            );
            0
        }
        Ok(BootstrapOutcome::InstalledNotStarted {
            binary,
            version,
            log_path,
            detail,
        }) => {
            println!(
                "{}",
                tf(
                    lang,
                    "engine.notstarted",
                    &[
                        ("version", &version),
                        ("path", &binary.display().to_string()),
                        ("log", &log_path.display().to_string()),
                        ("detail", &detail),
                    ],
                )
            );
            1
        }
        Err(reason) => {
            println!("{}", tf(lang, "engine.failed", &[("reason", &reason)]));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;

    // ---------------- pure helpers ----------------

    #[test]
    fn test_platform_engine_asset() {
        assert_eq!(
            platform_engine_asset("linux", "x86_64"),
            Some("ollama-linux-amd64.tar.zst")
        );
        assert_eq!(
            platform_engine_asset("linux", "aarch64"),
            Some("ollama-linux-arm64.tar.zst")
        );
        // One universal macOS build serves both architectures.
        assert_eq!(
            platform_engine_asset("macos", "x86_64"),
            Some("ollama-darwin.tgz")
        );
        assert_eq!(
            platform_engine_asset("macos", "aarch64"),
            Some("ollama-darwin.tgz")
        );
        assert_eq!(
            platform_engine_asset("windows", "x86_64"),
            Some("ollama-windows-amd64.zip")
        );
        assert_eq!(
            platform_engine_asset("windows", "aarch64"),
            Some("ollama-windows-arm64.zip")
        );
        // Not published by Ollama: no guessing.
        assert_eq!(platform_engine_asset("linux", "arm"), None);
        assert_eq!(platform_engine_asset("freebsd", "x86_64"), None);
        assert_eq!(platform_engine_asset("windows", "x86"), None);
    }

    #[test]
    fn test_human_size() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2048), "2.0 KiB (2048 bytes)");
        assert!(
            human_size(1_545_767_640).starts_with("1.4 GiB"),
            "{}",
            human_size(1_545_767_640)
        );
        assert!(human_size(186_116_182).starts_with("177.5 MiB"));
    }

    #[test]
    fn test_is_loopback_host() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("ollama.example.com"));
        assert!(!is_loopback_host("127.0.0.1.evil.com"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn test_extract_blocker_matrix() {
        // No tar at all blocks everything.
        assert!(extract_blocker("ollama-darwin.tgz", None, true).is_some());
        // gzip/zip assets need nothing extra.
        assert_eq!(
            extract_blocker("ollama-darwin.tgz", Some("tar (GNU tar) 1.35"), false),
            None
        );
        assert_eq!(
            extract_blocker("ollama-windows-amd64.zip", Some("bsdtar 3.5"), false),
            None
        );
        // .tar.zst: GNU tar needs the zstd program; bsdtar has it built in.
        assert!(extract_blocker(
            "ollama-linux-amd64.tar.zst",
            Some("tar (GNU tar) 1.35"),
            false
        )
        .is_some());
        assert_eq!(
            extract_blocker(
                "ollama-linux-amd64.tar.zst",
                Some("tar (GNU tar) 1.35"),
                true
            ),
            None
        );
        assert_eq!(
            extract_blocker("ollama-linux-amd64.tar.zst", Some("bsdtar 3.5"), false),
            None
        );
    }

    #[test]
    fn test_format_version() {
        assert_eq!(format_version((0, 40, 16)), "0.40.16");
        assert_eq!(format_version((9, 9, 9)), "9.9.9");
    }

    // ---------------- fake HTTP server ----------------

    fn http200(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn spawn_server(listener: TcpListener, routes: Vec<(String, Vec<u8>)>) {
        let routes = Arc::new(routes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = Arc::clone(&routes);
                std::thread::spawn(move || {
                    use std::io::Read as _;
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if crate::http::find_subslice(&buf, b"\r\n\r\n").is_some() {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let resp = routes
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_else(|| {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        });
                    let _ = stream.write_all(&resp);
                    let _ = stream.flush();
                });
            }
        });
    }

    fn fake_server(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        spawn_server(listener, routes);
        format!("http://127.0.0.1:{port}")
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pasture-bootstrap-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------------- streaming download ----------------

    #[test]
    fn test_download_to_file_content_length() {
        // 200 KB: several internal buffer refills, hashed on the way.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let base = fake_server(vec![("/dl".to_string(), http200(&payload))]);
        let dir = unique_temp_dir("dlcl");
        let dest = dir.join("archive.bin");
        let mut reports = Vec::new();
        let (n, sha) = download_to_file(
            &format!("{base}/dl"),
            &dest,
            1024 * 1024,
            &mut |done, _total| reports.push(done),
        )
        .unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(sha, crate::update::sha256_hex(&payload));
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert_eq!(reports.last(), Some(&(payload.len() as u64)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_to_file_chunked_and_redirect() {
        let payload = b"streamed engine bytes, chunk by chunk";
        let mut chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        chunked.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
        chunked.extend_from_slice(payload);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let base = fake_server(vec![
            ("/chunked".to_string(), chunked),
            (
                "/old".to_string(),
                b"HTTP/1.1 302 Found\r\nLocation: /chunked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
        ]);
        let dir = unique_temp_dir("dlchunk");
        let dest = dir.join("archive.bin");
        let (n, sha) =
            download_to_file(&format!("{base}/old"), &dest, 1024 * 1024, &mut |_, _| {}).unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(sha, crate::update::sha256_hex(payload));
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_to_file_cap_and_error_remove_partial() {
        let payload = vec![7u8; 50_000];
        let base = fake_server(vec![("/dl".to_string(), http200(&payload))]);
        let dir = unique_temp_dir("dlcap");
        let dest = dir.join("archive.bin");
        // Cap below the payload size: error, and no partial file.
        let err =
            download_to_file(&format!("{base}/dl"), &dest, 10_000, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("cap"), "{err}");
        assert!(!dest.exists(), "a capped download must not leave a file");
        // HTTP error: same guarantee.
        let err = download_to_file(
            &format!("{base}/missing"),
            &dest,
            1024 * 1024,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- release plan & checksum policy ----------------

    /// Serve a fake Ollama v9.9.9 release: the linux asset's digest is
    /// `archive_hash`; the sums asset, when present, carries `sums`.
    fn serve_engine_release(archive_hash: &str, sums: Option<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let mut assets = format!(
            r#"{{"name":"ollama-linux-amd64.tar.zst","browser_download_url":"{base}/dl/ollama-linux-amd64.tar.zst","digest":"sha256:{archive_hash}","size":1545767640}}"#
        );
        if sums.is_some() {
            assets.push_str(&format!(
                r#",{{"name":"sha256sum.txt","browser_download_url":"{base}/dl/sha256sum.txt"}}"#
            ));
        }
        let json = format!(r#"{{"tag_name":"v9.9.9","assets":[{assets}]}}"#);
        let mut routes = vec![("/releases/latest".to_string(), http200(json.as_bytes()))];
        if let Some(sums) = sums {
            routes.push(("/dl/sha256sum.txt".to_string(), http200(sums.as_bytes())));
        }
        spawn_server(listener, routes);
        base
    }

    #[test]
    fn test_fetch_engine_plan_uses_published_sums() {
        let hash = crate::update::sha256_hex(b"fake engine archive");
        let sums = format!("{hash}  ollama-linux-amd64.tar.zst\n");
        let base = serve_engine_release(&hash, Some(sums));
        let plan = fetch_engine_plan(&base, "linux", "x86_64").unwrap();
        assert_eq!(plan.tag, "v9.9.9");
        assert_eq!(plan.version, (9, 9, 9));
        assert_eq!(plan.asset_name, "ollama-linux-amd64.tar.zst");
        assert_eq!(plan.size_bytes, Some(1_545_767_640));
        assert_eq!(plan.expected_sha256, hash);
        assert!(plan
            .download_url
            .ends_with("/dl/ollama-linux-amd64.tar.zst"));
    }

    #[test]
    fn test_fetch_engine_plan_digest_fallback_and_disagreement() {
        let hash = crate::update::sha256_hex(b"fake engine archive");
        // No sums asset: the API digest alone is enough (Ollama's
        // digests and sums agree; the policy is update's, IMP-77).
        let base = serve_engine_release(&hash, None);
        let plan = fetch_engine_plan(&base, "linux", "x86_64").unwrap();
        assert_eq!(plan.expected_sha256, hash);
        // Sums present but disagreeing with the digest: refuse.
        let other = crate::update::sha256_hex(b"a different archive");
        let sums = format!("{other}  ollama-linux-amd64.tar.zst\n");
        let base = serve_engine_release(&hash, Some(sums));
        let err = fetch_engine_plan(&base, "linux", "x86_64").unwrap_err();
        assert!(err.contains("disagree"), "{err}");
        // Unsupported platform: a clear refusal, no asset guessing.
        let err = fetch_engine_plan(&base, "freebsd", "x86_64").unwrap_err();
        assert!(err.contains("ollama.com"), "{err}");
    }

    // ---------------- binary location & version ----------------

    #[test]
    fn test_find_engine_binary_layouts() {
        let dir = unique_temp_dir("find");
        assert_eq!(find_engine_binary(&dir), None);
        // Linux layout: bin/ollama with siblings under lib/.
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join("ollama");
        std::fs::write(&bin, "binary").unwrap();
        std::fs::create_dir_all(dir.join("lib/ollama")).unwrap();
        assert_eq!(find_engine_binary(&dir), Some(bin));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_managed_ollama_binary_picks_newest() {
        let home = unique_temp_dir("managed");
        assert_eq!(managed_ollama_binary(&home), None);
        for tag in ["ollama-v0.1.0", "ollama-v0.2.0"] {
            let bin_dir = home.join(tag).join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            std::fs::write(bin_dir.join("ollama"), "binary").unwrap();
        }
        // A versioned directory without a binary does not count.
        std::fs::create_dir_all(home.join("ollama-v0.3.0")).unwrap();
        let found = managed_ollama_binary(&home).unwrap();
        assert!(
            found.starts_with(home.join("ollama-v0.2.0")),
            "{}",
            found.display()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn test_ollama_binary_version_parsing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("ver");
        let script = dir.join("ollama");
        std::fs::write(&script, "#!/bin/sh\necho \"ollama version is 9.9.9\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(ollama_binary_version(&script), Some((9, 9, 9)));
        // A warning line before the version is tolerated.
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"Warning: something old\"\necho \"ollama version is 1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(ollama_binary_version(&script), Some((1, 2, 3)));
        assert_eq!(ollama_binary_version(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod e2e_tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;

    fn http200(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn spawn_server(listener: TcpListener, routes: Vec<(String, Vec<u8>)>) {
        let routes = Arc::new(routes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = Arc::clone(&routes);
                std::thread::spawn(move || {
                    use std::io::Read as _;
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if crate::http::find_subslice(&buf, b"\r\n\r\n").is_some() {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let resp = routes
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_else(|| {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .to_vec()
                        });
                    let _ = stream.write_all(&resp);
                    let _ = stream.flush();
                });
            }
        });
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pasture-bootstrap-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    const FAKE_SERVE_PY: &str = r#"
import os, socket
hostport = os.environ.get("OLLAMA_HOST", "127.0.0.1:11434")
host, port = hostport.rsplit(":", 1)
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind((host, int(port)))
srv.listen(8)
while True:
    conn, _ = srv.accept()
    try:
        conn.recv(4096)
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
    except OSError:
        pass
    finally:
        conn.close()
"#;

    /// Build a fake engine archive: `bin/ollama` (a shell script whose
    /// --version reports `reported` and whose serve runs a tiny python
    /// HTTP responder) plus a lib/ sibling, tarred and gzipped.
    /// Returns (archive bytes, sha256 hex).
    fn build_fixture_archive(dir: &Path, reported: &str) -> (Vec<u8>, String) {
        use std::os::unix::fs::PermissionsExt;
        let src = dir.join("src");
        let bin_dir = src.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(src.join("lib/ollama")).unwrap();
        std::fs::write(src.join("lib/ollama/placeholder"), "x").unwrap();
        let script = bin_dir.join("ollama");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"ollama version is {reported}\" ;;\n  serve) exec python3 \"$(dirname \"$0\")/fake_serve.py\" ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(bin_dir.join("fake_serve.py"), FAKE_SERVE_PY).unwrap();
        let archive = dir.join("engine.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("bin")
            .arg("lib")
            .status()
            .unwrap();
        assert!(status.success(), "tar must be available for this test");
        let bytes = std::fs::read(&archive).unwrap();
        let hash = crate::update::sha256_hex(&bytes);
        (bytes, hash)
    }

    /// Serve a v9.9.9 release whose darwin asset is `archive` with the
    /// given API digest (sums agree when attached).
    fn serve_full_release(archive: Vec<u8>, digest: &str, with_sums: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let size = archive.len();
        let mut assets = format!(
            r#"{{"name":"ollama-darwin.tgz","browser_download_url":"{base}/dl/ollama-darwin.tgz","digest":"sha256:{digest}","size":{size}}}"#
        );
        if with_sums {
            assets.push_str(&format!(
                r#",{{"name":"sha256sum.txt","browser_download_url":"{base}/dl/sha256sum.txt"}}"#
            ));
        }
        let json = format!(r#"{{"tag_name":"v9.9.9","assets":[{assets}]}}"#);
        let mut routes = vec![
            ("/releases/latest".to_string(), http200(json.as_bytes())),
            ("/dl/ollama-darwin.tgz".to_string(), http200(&archive)),
        ];
        if with_sums {
            let real = crate::update::sha256_hex(&archive);
            routes.push((
                "/dl/sha256sum.txt".to_string(),
                http200(format!("{real}  ollama-darwin.tgz\n").as_bytes()),
            ));
        }
        spawn_server(listener, routes);
        base
    }

    fn plan_for(base: &str, home: &Path, host: &str, port: u16, check_only: bool) -> BootstrapPlan {
        BootstrapPlan {
            api_base: base.to_string(),
            engine_home: home.to_path_buf(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            ollama_host: host.to_string(),
            ollama_port: port,
            check_only,
            lang: crate::i18n::Lang::En,
        }
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn test_perform_bootstrap_end_to_end() {
        if which_on_path("ollama").is_some() || !python3_available() {
            // The fixture must be the only engine source, and its
            // `serve` needs python3 for the responder.
            return;
        }
        let dir = unique_temp_dir("e2e");
        let (archive, hash) = build_fixture_archive(&dir, "9.9.9");
        let base = serve_full_release(archive, &hash, true);
        let home = dir.join("engines");
        let port = free_port();
        let plan = plan_for(&base, &home, "127.0.0.2", port, false);
        let mut logs = Vec::new();
        let outcome =
            perform_bootstrap(&plan, &mut |l| logs.push(l.to_string()), &mut |_, _| {}).unwrap();
        match outcome {
            BootstrapOutcome::Started {
                binary,
                version,
                freshly_installed,
                ..
            } => {
                assert_eq!(version, "9.9.9");
                assert!(freshly_installed);
                assert!(
                    binary.starts_with(home.join("ollama-v9.9.9")),
                    "{}",
                    binary.display()
                );
            }
            other => panic!("expected Started, got {other:?}"),
        }
        // The plan was announced before the download (source + size).
        assert!(
            logs.iter()
                .any(|l| l.contains("ollama-darwin.tgz") || l.contains("/dl/")),
            "plan announcement missing: {logs:?}"
        );
        // No staging leftovers.
        let leftovers: Vec<String> = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(".staging"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Stop the fixture server process (it outlives the command by
        // design; the test must not leak it).
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg("fake_serve.py")
            .status();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_perform_bootstrap_check_only_installs_nothing() {
        if which_on_path("ollama").is_some() {
            return;
        }
        let dir = unique_temp_dir("checkonly");
        let (archive, hash) = build_fixture_archive(&dir, "9.9.9");
        let base = serve_full_release(archive, &hash, true);
        let home = dir.join("engines");
        let plan = plan_for(&base, &home, "127.0.0.3", free_port(), true);
        let outcome = perform_bootstrap(&plan, &mut |_| {}, &mut |_, _| {}).unwrap();
        match outcome {
            BootstrapOutcome::WouldInstall { plan } => {
                assert_eq!(plan.tag, "v9.9.9");
                assert_eq!(plan.expected_sha256, hash);
            }
            other => panic!("expected WouldInstall, got {other:?}"),
        }
        assert!(
            !home.exists() || std::fs::read_dir(&home).unwrap().next().is_none(),
            "--check must not create an install"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_perform_bootstrap_checksum_mismatch_installs_nothing() {
        if which_on_path("ollama").is_some() {
            return;
        }
        let dir = unique_temp_dir("badhash");
        let (archive, _real) = build_fixture_archive(&dir, "9.9.9");
        let wrong = crate::update::sha256_hex(b"not the archive");
        // Sums agree with the (wrong) digest: the served bytes match
        // neither, so the download itself fails verification.
        let base = serve_full_release(archive, &wrong, false);
        let home = dir.join("engines");
        let plan = plan_for(&base, &home, "127.0.0.4", free_port(), false);
        let err = perform_bootstrap(&plan, &mut |_| {}, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(managed_ollama_binary(&home).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_perform_bootstrap_version_mismatch_installs_nothing() {
        if which_on_path("ollama").is_some() {
            return;
        }
        let dir = unique_temp_dir("badver");
        // The archive is intact (digest matches) but the binary inside
        // reports a different version than the release claims.
        let (archive, hash) = build_fixture_archive(&dir, "9.9.8");
        let base = serve_full_release(archive, &hash, true);
        let home = dir.join("engines");
        let plan = plan_for(&base, &home, "127.0.0.5", free_port(), false);
        let err = perform_bootstrap(&plan, &mut |_| {}, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("reports version"), "{err}");
        assert!(managed_ollama_binary(&home).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_perform_bootstrap_already_running() {
        // Something answering HTTP on Ollama's own port counts as a
        // running engine; discovery must short-circuit before any
        // install work. Skips when the port cannot be bound (a real
        // engine is already there — the same verdict by other means,
        // but not this test's to claim).
        let Ok(listener) = TcpListener::bind("127.0.0.1:11434") else {
            return;
        };
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::Read as _;
                    let mut buf = [0u8; 512];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                    let _ = stream.flush();
                });
            }
        });
        let dir = unique_temp_dir("running");
        let home = dir.join("engines");
        let plan = plan_for("http://127.0.0.1:1", &home, "127.0.0.1", 11434, false);
        let outcome = perform_bootstrap(&plan, &mut |_| {}, &mut |_, _| {}).unwrap();
        match outcome {
            BootstrapOutcome::AlreadyRunning { name, .. } => assert_eq!(name, "Ollama"),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

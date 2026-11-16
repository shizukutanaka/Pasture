//! Self-update for the prebuilt release binaries (IMP-77, ADR-286).
//!
//! `pasture update` is the only command that talks to GitHub, and only
//! when the user runs it: there is no background check, no scheduled
//! phone-home, and nothing about the user is sent — the requests are
//! anonymous GETs for the public release metadata and the archive itself.
//! A binary that is never asked never touches the network for updates.
//!
//! Trust model: the release metadata comes from the GitHub API over TLS.
//! A downloaded archive is installed only after its SHA-256 matches the
//! value published with the release — the `SHA256SUMS` asset when one is
//! attached (the project's checksum practice), otherwise the per-asset
//! `digest` the API reports. If neither is available, or the two
//! disagree, the update refuses and the installed binary is left
//! untouched. The new binary must also run and report the expected
//! version before it replaces anything.
//!
//! Only binaries built by release.yml carry the release-build marker
//! (`PASTURE_RELEASE_BUILD`, read at compile time via `option_env!`); a
//! source build explains how to update from source instead of replacing
//! itself — silently swapping a developer's own build for a release
//! binary would be a surprise, not a service.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Where release metadata comes from when `PASTURE_UPDATE_API_BASE` is
/// unset: the project's own GitHub Releases API.
pub const DEFAULT_API_BASE: &str = "https://api.github.com/repos/shizukutanaka/Pasture";

/// Name of the checksums asset release.yml attaches to each release.
const SUMS_ASSET: &str = "SHA256SUMS";

/// Cap on anything the updater downloads (release JSON, sums, archive).
/// Real archives are ~0.5 MB; the cap exists so a broken or hostile
/// endpoint cannot make the updater buffer unbounded data.
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

/// True only for binaries built by release.yml, which sets
/// `PASTURE_RELEASE_BUILD=1` in the build environment.
pub fn is_release_build() -> bool {
    option_env!("PASTURE_RELEASE_BUILD").is_some()
}

/// Base URL of the releases API (no trailing slash). Overridable for
/// mirrors and for tests, which point it at a local fake server.
pub fn api_base() -> String {
    match std::env::var("PASTURE_UPDATE_API_BASE") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => DEFAULT_API_BASE.to_string(),
    }
}

/// Parse a release tag or version string ("v0.29.0", "0.29.0") into a
/// comparable numeric triple. A pre-release/build suffix does not take
/// part in ordering (published tags here are plain `v<semver>`); anything
/// that does not parse yields None rather than a guessed ordering.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Map an (OS, arch) pair to the release-asset platform suffix and
/// archive extension release.yml publishes. None for anything release.yml
/// does not build (Intel macOS, ARM Linux, ...): those users update
/// manually, and the updater says so instead of guessing an asset name.
pub fn platform_asset(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    match (os, arch) {
        ("linux", "x86_64") => Some(("linux-x86_64", "tar.gz")),
        ("windows", "x86_64") => Some(("windows-x86_64", "zip")),
        ("macos", "aarch64") => Some(("macos-aarch64", "tar.gz")),
        _ => None,
    }
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    /// SHA-256 the API reports for the asset (its `digest` field,
    /// "sha256:<hex>"), when present.
    pub digest_sha256: Option<String>,
}

/// The parts of a GitHub release the updater needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: (u64, u64, u64),
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseInfo {
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Parse the JSON body of `GET /repos/{owner}/{repo}/releases/latest`.
pub fn parse_release(body: &str) -> Result<ReleaseInfo, String> {
    let root =
        crate::json::parse(body).map_err(|e| format!("release metadata is not valid JSON: {e}"))?;
    let tag = root
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("release metadata has no tag_name")?
        .to_string();
    let version = parse_version(&tag)
        .ok_or_else(|| format!("cannot parse the latest release tag ({tag})"))?;
    let mut assets = Vec::new();
    if let Some(list) = root.get("assets").and_then(|v| v.as_array()) {
        for item in list {
            let (Some(name), Some(url)) = (
                item.get("name").and_then(|v| v.as_str()),
                item.get("browser_download_url").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let digest_sha256 = item
                .get("digest")
                .and_then(|v| v.as_str())
                .and_then(|d| d.strip_prefix("sha256:"))
                .filter(|d| is_sha256_hex(d))
                .map(|d| d.to_ascii_lowercase());
            assets.push(ReleaseAsset {
                name: name.to_string(),
                download_url: url.to_string(),
                digest_sha256,
            });
        }
    }
    Ok(ReleaseInfo {
        tag,
        version,
        assets,
    })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse a sha256sum-format listing into (lowercase hex digest, file
/// name) pairs. Malformed lines are skipped; whether a missing entry is
/// fatal is the caller's decision (for the asset being installed, it is).
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(sp) = line.find(char::is_whitespace) else {
            continue;
        };
        let hash = &line[..sp];
        // After the hash: whitespace, then the name, optionally preceded
        // by '*' (sha256sum's binary-mode marker) and/or a './' prefix
        // (Ollama's published sha256sum.txt lists `./ollama-...`; the
        // name compared against is the bare asset name).
        let name = line[sp..]
            .trim()
            .trim_start_matches('*')
            .trim()
            .trim_start_matches("./");
        if is_sha256_hex(hash) && !name.is_empty() {
            out.push((hash.to_ascii_lowercase(), name.to_string()));
        }
    }
    out
}

/// Decide the SHA-256 a downloaded archive must match.
///
/// The release's `SHA256SUMS` asset is the project's published checksum
/// record and wins when attached. The API's per-asset digest is the
/// fallback (the v0.28.0 sums file agrees with it — it was generated from
/// the same assets). If both exist and disagree, or neither exists,
/// there is no trustworthy value and the update must refuse.
pub fn expected_sha256(
    release: &ReleaseInfo,
    asset_name: &str,
    sums_text: Option<&str>,
) -> Result<String, String> {
    let from_sums = match sums_text {
        Some(text) => Some(
            parse_sha256sums(text)
                .iter()
                .find(|(_, name)| name == asset_name)
                .map(|(hash, _)| hash.clone())
                .ok_or_else(|| format!("SHA256SUMS has no entry for {asset_name}"))?,
        ),
        None => None,
    };
    let from_api = release
        .asset(asset_name)
        .and_then(|a| a.digest_sha256.clone());
    match (from_sums, from_api) {
        (Some(s), Some(a)) if s == a => Ok(s),
        (Some(_), Some(_)) => Err(
            "SHA256SUMS and the digest reported by the API disagree; refusing to install"
                .to_string(),
        ),
        (Some(s), None) => Ok(s),
        (None, Some(a)) => Ok(a),
        (None, None) => Err(
            "the release publishes no checksum for this asset; refusing to install an \
             unverifiable download"
                .to_string(),
        ),
    }
}

// ---------------------------------------------------------------------
// SHA-256 (FIPS 180-4), implemented in-crate so the default build keeps
// invariant I1 (zero non-std dependencies). Streaming: feed chunks with
// `update`, finish with `finalize_hex`.
// ---------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        self.absorb(data);
    }

    /// Buffer and compress without touching the message-length counter
    /// (padding bytes are not part of the message).
    fn absorb(&mut self, mut data: &[u8]) {
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize_hex(mut self) -> String {
        let bit_len = self.total_len.wrapping_mul(8);
        self.absorb(&[0x80]);
        while self.buf_len != 56 {
            self.absorb(&[0]);
        }
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        self.compress(&block);
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
            self.state[5],
            self.state[6],
            self.state[7],
        );
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

/// One-shot SHA-256 of an in-memory buffer, as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize_hex()
}

/// SHA-256 of a file, streamed so archives never sit in memory twice.
pub fn sha256_file_hex(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&chunk[..n]);
    }
    Ok(h.finalize_hex())
}

// ---------------------------------------------------------------------
// Minimal HTTP(S) GET client: the updater's only network primitive.
// Everything it fetches is a public release artifact. Plain HTTP is
// supported so tests (and local mirrors) can use a local server; the
// production endpoints are HTTPS, which needs the `cloud` feature's TLS
// stack — release binaries are built with it (ADR-285).
// ---------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// GET `url`, following redirects, and return the body bytes.
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let mut current = url.to_string();
    for _ in 0..6 {
        let resp = fetch_once(&current)?;
        match resp.status {
            200..=299 => {
                if resp.body.len() > MAX_DOWNLOAD_BYTES {
                    return Err(format!(
                        "download from {current} exceeds the {}-byte cap",
                        MAX_DOWNLOAD_BYTES
                    ));
                }
                return Ok(resp.body);
            }
            301 | 302 | 303 | 307 | 308 => {
                let location = resp
                    .header("location")
                    .ok_or("the server redirected without a Location header")?;
                current = resolve_redirect(&current, location);
            }
            status => return Err(format!("GET {current} failed: HTTP {status}")),
        }
    }
    Err("too many redirects".to_string())
}

/// Resolve a redirect target against the URL that produced it. GitHub's
/// download redirects are absolute; the relative forms are handled for
/// completeness (and for local test servers).
pub(crate) fn resolve_redirect(current: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let (scheme, rest) = current.split_once("://").unwrap_or(("https", current));
    let authority = rest.split('/').next().unwrap_or(rest);
    if let Some(stripped) = location.strip_prefix("//") {
        return format!("{scheme}://{stripped}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    let base = match current.rfind('/') {
        Some(i) => &current[..i + 1],
        None => current,
    };
    format!("{base}{location}")
}

fn fetch_once(url: &str) -> Result<HttpResponse, String> {
    let (scheme, host, port, path) = parse_url(url)?;
    let host_header = if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: pasture/{} (pasture update)\r\nAccept: application/vnd.github+json, application/octet-stream\r\nConnection: close\r\n\r\n",
        crate::VERSION
    );
    let mut raw = Vec::new();
    match scheme.as_str() {
        "http" => {
            let mut stream = tcp_connect(&host, port)?;
            stream
                .write_all(request.as_bytes())
                .map_err(|e| format!("send to {host}: {e}"))?;
            stream
                .read_to_end(&mut raw)
                .map_err(|e| format!("read from {host}: {e}"))?;
        }
        "https" => https_exchange_bytes(&host, port, request.as_bytes(), &mut raw)?,
        other => return Err(format!("unsupported URL scheme: {other}")),
    }
    parse_http_bytes(&raw)
}

fn tcp_connect(host: &str, port: u16) -> Result<std::net::TcpStream, String> {
    let stream = std::net::TcpStream::connect((host, port))
        .map_err(|e| format!("connect to {host}:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    Ok(stream)
}

#[cfg(feature = "cloud")]
fn https_exchange_bytes(
    host: &str,
    port: u16,
    request: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let connector = native_tls::TlsConnector::new().map_err(|e| format!("tls init: {e}"))?;
    let tcp = tcp_connect(host, port)?;
    let mut stream = connector
        .connect(host, tcp)
        .map_err(|e| format!("tls handshake with {host}: {e}"))?;
    stream
        .write_all(request)
        .map_err(|e| format!("send to {host}: {e}"))?;
    stream
        .read_to_end(out)
        .map_err(|e| format!("read from {host}: {e}"))?;
    Ok(())
}

#[cfg(not(feature = "cloud"))]
fn https_exchange_bytes(
    host: &str,
    _port: u16,
    _request: &[u8],
    _out: &mut [u8],
) -> Result<(), String> {
    Err(format!(
        "this build has no TLS support, so it cannot reach {host}; self-update over \
         HTTPS is available in the prebuilt release binaries"
    ))
}

/// Split a URL into (scheme, host, port, path). Deliberately small: the
/// updater only ever talks to a releases API and its download hosts.
pub(crate) fn parse_url(url: &str) -> Result<(String, String, u16, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("malformed URL: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in URL: {url}"))?,
        ),
        _ => (
            authority.to_string(),
            if scheme == "https" { 443 } else { 80 },
        ),
    };
    Ok((scheme.to_string(), host, port, path.to_string()))
}

/// Parse a raw HTTP/1.x response (status line, headers, body), decoding
/// chunked transfer encoding when present.
fn parse_http_bytes(raw: &[u8]) -> Result<HttpResponse, String> {
    let split = crate::http::find_subslice(raw, b"\r\n\r\n")
        .ok_or("malformed HTTP response (no header terminator)")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty HTTP response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed HTTP status line: {status_line}"))?;
    if (100..200).contains(&status) {
        // Interim response (e.g. 100 Continue): the real one follows.
        return parse_http_bytes(&raw[split + 4..]);
    }
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let body_bytes = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        dechunk(body_bytes)?
    } else {
        body_bytes.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Decode a chunked transfer-encoded body.
fn dechunk(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let eol =
            crate::http::find_subslice(rest, b"\r\n").ok_or("chunked body: missing chunk size")?;
        let size_line = String::from_utf8_lossy(&rest[..eol]).into_owned();
        let size =
            usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0").trim(), 16)
                .map_err(|_| "chunked body: bad chunk size".to_string())?;
        if size == 0 {
            break;
        }
        let data = &rest[eol + 2..];
        if data.len() < size {
            return Err("chunked body truncated".to_string());
        }
        out.extend_from_slice(&data[..size]);
        rest = &data[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
        if out.len() > MAX_DOWNLOAD_BYTES {
            return Err("download exceeds the size cap".to_string());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Extraction and replacement.
// ---------------------------------------------------------------------

/// Extract a downloaded release archive with the system `tar` (present
/// on Linux, macOS and Windows 10+; bsdtar auto-detects .tar.gz and
/// .zip, as does GNU tar for gzip). Failure is loud and leaves the
/// installed binary untouched.
pub(crate) fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    let output = std::process::Command::new("tar")
        // Never restore archive ownership: extraction targets a fresh
        // staging directory, and as root (containers, some CI) tar
        // would otherwise try to chown to the archive's uid/gid and
        // fail on ids the sandbox cannot grant (hit by IMP-78's smoke
        // against a fixture with a foreign gid). Permissions — the
        // executable bit the payload needs — are unaffected.
        .arg("--no-same-owner")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .output()
        .map_err(|e| {
            format!(
                "could not run `tar` to extract the update ({e}); download the release \
                 manually from https://github.com/shizukutanaka/Pasture/releases"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`tar` failed to extract the update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Locate the pasture binary inside an extracted release archive
/// (layout: `pasture-<tag>-<platform>/pasture[.exe]`).
pub fn find_extracted_binary(dir: &Path) -> Option<PathBuf> {
    find_named(dir, 0)
}

fn find_named(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "pasture" || name == "pasture.exe" {
                return Some(path);
            }
        }
    }
    for sub in subdirs {
        if let Some(found) = find_named(&sub, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Run `<path> version` and return the version it reports ("pasture
/// X.Y.Z" → "X.Y.Z"). The downloaded binary must prove it is the release
/// it claims to be before it is allowed to replace anything.
pub fn binary_reported_version(path: &Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().nth(1).map(|s| s.to_string())
}

/// Outcome of a successful binary replacement.
#[derive(Debug)]
pub struct ReplaceOutcome {
    /// On Windows, the previous binary renamed aside (it cannot be
    /// deleted while it is the running process; a later run removes it).
    /// None on other platforms, where the swap is a single rename.
    pub parked_old: Option<PathBuf>,
}

/// Replace `exe` with `new_binary`. The new file is staged in the same
/// directory first, so the swap itself is a rename within one filesystem
/// and the installed binary is never half-written. On any error the
/// original binary is left in place.
pub fn replace_binary(exe: &Path, new_binary: &Path) -> Result<ReplaceOutcome, String> {
    let dir = exe
        .parent()
        .ok_or("the current executable has no parent directory")?;
    let staged = dir.join(format!(".pasture-update-{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(new_binary, &staged)
        .map_err(|e| format!("cannot stage the new binary in {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let outcome = swap_in_place(exe, &staged);
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    outcome
}

#[cfg(not(windows))]
fn swap_in_place(exe: &Path, staged: &Path) -> Result<ReplaceOutcome, String> {
    // A rename over the running executable is atomic and safe on Unix:
    // the running process keeps the old inode, new invocations get the
    // new file.
    std::fs::rename(staged, exe).map_err(|e| format!("cannot replace {}: {e}", exe.display()))?;
    Ok(ReplaceOutcome { parked_old: None })
}

#[cfg(windows)]
fn swap_in_place(exe: &Path, staged: &Path) -> Result<ReplaceOutcome, String> {
    // Windows cannot overwrite a running executable, but it can rename
    // one: park the current binary as `<exe>.old`, move the new one in,
    // and let a later run delete the parked file (cleanup_stale_old).
    let mut old_os = exe.as_os_str().to_owned();
    old_os.push(".old");
    let old = PathBuf::from(old_os);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("cannot move the current binary aside: {e}"))?;
    match std::fs::rename(staged, exe) {
        Ok(()) => Ok(ReplaceOutcome {
            parked_old: Some(old),
        }),
        Err(e) => {
            // Roll back: an install must never be left without a binary.
            let _ = std::fs::rename(&old, exe);
            Err(format!(
                "cannot install the new binary ({e}); the previous binary was restored"
            ))
        }
    }
}

/// Best-effort removal of a parked `<exe>.old` left by a Windows update.
/// Called on every startup; a no-op on other platforms and whenever the
/// file is absent or still locked.
pub fn cleanup_stale_old() {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let mut old_os = exe.as_os_str().to_owned();
            old_os.push(".old");
            let _ = std::fs::remove_file(PathBuf::from(old_os));
        }
    }
}

/// Fail fast when the install directory is not writable, so the user
/// learns about it before anything is downloaded.
fn probe_writable(exe: &Path) -> Result<(), String> {
    let dir = exe
        .parent()
        .ok_or("the current executable has no parent directory")?;
    let probe = dir.join(format!(".pasture-update-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| {
        format!(
            "cannot write to the install directory {} ({e}); move the pasture binary \
             somewhere writable, or update it manually",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

// ---------------------------------------------------------------------
// Orchestration.
// ---------------------------------------------------------------------

/// Inputs to an update run, gathered from the real environment by
/// `run_update` and fabricated by tests.
pub struct UpdatePlan {
    pub api_base: String,
    pub current_version: String,
    pub exe_path: PathBuf,
    pub os: String,
    pub arch: String,
    pub check_only: bool,
}

#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate,
    Available {
        latest: String,
    },
    Updated {
        from: String,
        to: String,
        parked_old: Option<PathBuf>,
    },
}

/// Check for (and, unless `check_only`, install) the newest release.
/// Every failure returns a user-readable reason; the installed binary is
/// modified only at the very last step, after the download has been
/// hash-verified and the new binary has reported the expected version.
pub fn perform_update(plan: &UpdatePlan) -> Result<UpdateOutcome, String> {
    let (suffix, ext) = platform_asset(&plan.os, &plan.arch).ok_or_else(|| {
        format!(
            "self-update is not available for {}-{}; download the latest release from \
             https://github.com/shizukutanaka/Pasture/releases",
            plan.os, plan.arch
        )
    })?;
    let current = parse_version(&plan.current_version).ok_or_else(|| {
        format!(
            "cannot parse the current version ({})",
            plan.current_version
        )
    })?;
    let meta = fetch(&format!("{}/releases/latest", plan.api_base))?;
    let release = parse_release(&String::from_utf8_lossy(&meta))?;
    if release.version <= current {
        return Ok(UpdateOutcome::UpToDate);
    }
    let latest = release.tag.trim_start_matches('v').to_string();
    if plan.check_only {
        return Ok(UpdateOutcome::Available { latest });
    }
    probe_writable(&plan.exe_path)?;
    let asset_name = format!("pasture-{}-{}.{}", release.tag, suffix, ext);
    let asset = release
        .asset(&asset_name)
        .ok_or_else(|| {
            format!(
                "the {} release has no asset for this platform ({asset_name})",
                release.tag
            )
        })?
        .clone();
    let sums_text = match release.asset(SUMS_ASSET) {
        Some(sums_asset) => {
            let bytes = fetch(&sums_asset.download_url)?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        None => None,
    };
    let expected = expected_sha256(&release, &asset_name, sums_text.as_deref())?;

    // Unique per run (pid + timestamp): two updates must never share a
    // work directory.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let workdir =
        std::env::temp_dir().join(format!("pasture-update-{}-{stamp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workdir);
    std::fs::create_dir_all(&workdir)
        .map_err(|e| format!("cannot create a work directory: {e}"))?;
    let result = install_download(plan, &release, &asset, &expected, &workdir);
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

fn install_download(
    plan: &UpdatePlan,
    release: &ReleaseInfo,
    asset: &ReleaseAsset,
    expected: &str,
    workdir: &Path,
) -> Result<UpdateOutcome, String> {
    let archive_path = workdir.join(&asset.name);
    let bytes = fetch(&asset.download_url)?;
    std::fs::write(&archive_path, bytes).map_err(|e| format!("cannot save the download: {e}"))?;
    let actual = sha256_file_hex(&archive_path)?;
    if !crate::http::constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        return Err(format!(
            "checksum mismatch for {} (expected {expected}, got {actual}); the download \
             was discarded and your current binary was not changed",
            asset.name
        ));
    }
    let extract_dir = workdir.join("extracted");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("cannot create a work directory: {e}"))?;
    extract_archive(&archive_path, &extract_dir)?;
    let new_binary = find_extracted_binary(&extract_dir)
        .ok_or("the downloaded archive does not contain a pasture binary")?;
    let reported = binary_reported_version(&new_binary)
        .ok_or("the downloaded binary did not run, so it was not installed")?;
    if parse_version(&reported) != Some(release.version) {
        return Err(format!(
            "the downloaded binary reports version {reported}, not {}; it was not installed",
            release.tag
        ));
    }
    let outcome = replace_binary(&plan.exe_path, &new_binary)?;
    Ok(UpdateOutcome::Updated {
        from: plan.current_version.clone(),
        to: release.tag.trim_start_matches('v').to_string(),
        parked_old: outcome.parked_old,
    })
}

/// `pasture update [--check]`: the CLI entry point. Returns an exit code.
pub fn run_update(check_only: bool) -> i32 {
    use crate::i18n::{t, tf};
    let lang = crate::i18n::detect();
    if !is_release_build() {
        println!("{}", t(lang, "update.devbuild"));
        return 1;
    }
    if platform_asset(std::env::consts::OS, std::env::consts::ARCH).is_none() {
        println!(
            "{}",
            tf(
                lang,
                "update.unsupported",
                &[(
                    "platform",
                    &format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
                )]
            )
        );
        return 1;
    }
    println!("{}", t(lang, "update.checking"));
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            println!(
                "{}",
                tf(
                    lang,
                    "update.failed",
                    &[(
                        "reason",
                        &format!("cannot locate the running executable: {e}")
                    )]
                )
            );
            return 1;
        }
    };
    let plan = UpdatePlan {
        api_base: api_base(),
        current_version: crate::VERSION.to_string(),
        exe_path,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        check_only,
    };
    match perform_update(&plan) {
        Ok(UpdateOutcome::UpToDate) => {
            println!(
                "{}",
                tf(lang, "update.uptodate", &[("version", crate::VERSION)])
            );
            0
        }
        Ok(UpdateOutcome::Available { latest }) => {
            println!(
                "{}",
                tf(
                    lang,
                    "update.available",
                    &[("current", crate::VERSION), ("latest", &latest)]
                )
            );
            0
        }
        Ok(UpdateOutcome::Updated {
            from,
            to,
            parked_old,
        }) => {
            println!(
                "{}",
                tf(lang, "update.done", &[("current", &from), ("latest", &to)])
            );
            if let Some(old) = parked_old {
                println!(
                    "{}",
                    tf(
                        lang,
                        "update.oldfile",
                        &[("path", &old.display().to_string())]
                    )
                );
            }
            0
        }
        Err(reason) => {
            println!("{}", tf(lang, "update.failed", &[("reason", &reason)]));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;

    // ---------------- SHA-256 ----------------

    #[test]
    fn test_sha256_known_vectors() {
        // FIPS 180-4 / RFC 6234 test vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes: the padding boundary (length lands in a second block).
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1,000,000 x 'a': many blocks.
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&million),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn test_sha256_chunked_updates_match_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let one_shot = sha256_hex(&data);
        for step in [1usize, 3, 63, 64, 65, 1000] {
            let mut h = Sha256::new();
            for chunk in data.chunks(step) {
                h.update(chunk);
            }
            assert_eq!(h.finalize_hex(), one_shot, "chunk size {step}");
        }
    }

    #[test]
    fn test_sha256_file_hex() {
        let dir = unique_temp_dir("hashfile");
        let path = dir.join("blob.bin");
        let data = b"pasture update checksum test payload";
        std::fs::write(&path, data).unwrap();
        assert_eq!(sha256_file_hex(&path).unwrap(), sha256_hex(data));
        assert!(sha256_file_hex(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- versions & platforms ----------------

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("v0.28.0"), Some((0, 28, 0)));
        assert_eq!(parse_version("0.29.1"), Some((0, 29, 1)));
        assert_eq!(parse_version("v1.0.0-rc.1"), Some((1, 0, 0)));
        assert_eq!(parse_version("  v2.3.4  "), Some((2, 3, 4)));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version(""), None);
        assert!(parse_version("v0.29.0") > parse_version("v0.28.0"));
        assert!(parse_version("v0.28.0") <= parse_version("0.28.0"));
    }

    #[test]
    fn test_platform_asset() {
        assert_eq!(
            platform_asset("linux", "x86_64"),
            Some(("linux-x86_64", "tar.gz"))
        );
        assert_eq!(
            platform_asset("windows", "x86_64"),
            Some(("windows-x86_64", "zip"))
        );
        assert_eq!(
            platform_asset("macos", "aarch64"),
            Some(("macos-aarch64", "tar.gz"))
        );
        // Not built by release.yml: no guessing.
        assert_eq!(platform_asset("macos", "x86_64"), None);
        assert_eq!(platform_asset("linux", "aarch64"), None);
        assert_eq!(platform_asset("freebsd", "x86_64"), None);
    }

    // ---------------- release metadata & checksums ----------------

    /// Modelled on the real v0.28.0 payload (names, digests).
    const RELEASE_JSON: &str = r#"{
      "tag_name": "v0.28.0",
      "name": "v0.28.0",
      "assets": [
        {"name": "pasture-v0.28.0-linux-x86_64.tar.gz",
         "browser_download_url": "https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/pasture-v0.28.0-linux-x86_64.tar.gz",
         "digest": "sha256:edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8"},
        {"name": "pasture-v0.28.0-macos-aarch64.tar.gz",
         "browser_download_url": "https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/pasture-v0.28.0-macos-aarch64.tar.gz",
         "digest": "sha256:f22641e744c3f6f55a9a6c149b894d097944b4734566f5a254e3934d69233c63"},
        {"name": "pasture-v0.28.0-windows-x86_64.zip",
         "browser_download_url": "https://github.com/shizukutanaka/Pasture/releases/download/v0.28.0/pasture-v0.28.0-windows-x86_64.zip",
         "digest": "sha256:89d38b2f78427f82c0ff5028b8bcbb8801686984037a0395be452d5b8ed11fc9"}
      ]
    }"#;

    #[test]
    fn test_parse_release() {
        let rel = parse_release(RELEASE_JSON).unwrap();
        assert_eq!(rel.tag, "v0.28.0");
        assert_eq!(rel.version, (0, 28, 0));
        assert_eq!(rel.assets.len(), 3);
        let linux = rel.asset("pasture-v0.28.0-linux-x86_64.tar.gz").unwrap();
        assert_eq!(
            linux.digest_sha256.as_deref(),
            Some("edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8")
        );
        assert!(rel.asset("SHA256SUMS").is_none());
        assert!(parse_release("not json").is_err());
        assert!(parse_release("{}").is_err());
        assert!(parse_release(r#"{"tag_name": "soon"}"#).is_err());
    }

    #[test]
    fn test_parse_sha256sums() {
        // The actual v0.28.0 sums file content.
        let text = "edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8  pasture-v0.28.0-linux-x86_64.tar.gz\n\
                    f22641e744c3f6f55a9a6c149b894d097944b4734566f5a254e3934d69233c63  pasture-v0.28.0-macos-aarch64.tar.gz\n\
                    89d38b2f78427f82c0ff5028b8bcbb8801686984037a0395be452d5b8ed11fc9  pasture-v0.28.0-windows-x86_64.zip\n";
        let entries = parse_sha256sums(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].1, "pasture-v0.28.0-linux-x86_64.tar.gz");
        // Binary-mode marker and CRLF are tolerated; garbage is skipped.
        let messy =
            "EDB51219F0B12852A6C2B6C61BB6BDB3910890DC337E7220C3284BF07B4A69A8 *a.tar.gz\r\n\
                     not-a-hash  b.tar.gz\n\
                     \n";
        let entries = parse_sha256sums(messy);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "a.tar.gz");
        assert!(entries[0].0.starts_with("edb5"));
        // A './' prefix on the name is stripped (Ollama's published
        // sha256sum.txt lists `./ollama-linux-amd64.tar.zst`; IMP-78's
        // engine bootstrap looks the bare asset name up in it).
        let dotted = "87bfafb2acbc89b2e6a1be0a9043de6aa75fb450696f86bc426e65fc448f788d  ./ollama-linux-amd64.tar.zst\n";
        let entries = parse_sha256sums(dotted);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "ollama-linux-amd64.tar.zst");
    }

    #[test]
    fn test_expected_sha256_policy() {
        let rel = parse_release(RELEASE_JSON).unwrap();
        let name = "pasture-v0.28.0-linux-x86_64.tar.gz";
        let digest = "edb51219f0b12852a6c2b6c61bb6bdb3910890dc337e7220c3284bf07b4a69a8";
        // Digest only (v0.28.0's current state: no sums asset attached).
        assert_eq!(expected_sha256(&rel, name, None).unwrap(), digest);
        // Sums only, agreeing.
        let sums = format!("{digest}  {name}\n");
        assert_eq!(expected_sha256(&rel, name, Some(&sums)).unwrap(), digest);
        // Both present but disagreeing: refuse.
        let other = sha256_hex(b"different bytes");
        let bad_sums = format!("{other}  {name}\n");
        assert!(expected_sha256(&rel, name, Some(&bad_sums)).is_err());
        // Sums attached but the asset is not listed: refuse.
        let empty_sums = format!("{other}  some-other-file.tar.gz\n");
        assert!(expected_sha256(&rel, name, Some(&empty_sums)).is_err());
        // Neither source: refuse.
        let bare = parse_release(r#"{"tag_name":"v1.0.0","assets":[{"name":"x.tar.gz","browser_download_url":"https://example.invalid/x.tar.gz"}]}"#).unwrap();
        assert!(expected_sha256(&bare, "x.tar.gz", None).is_err());
    }

    // ---------------- HTTP plumbing ----------------

    #[test]
    fn test_parse_url() {
        let (scheme, host, port, path) =
            parse_url("https://api.github.com/repos/x/releases/latest").unwrap();
        assert_eq!(
            (scheme.as_str(), host.as_str(), port, path.as_str()),
            ("https", "api.github.com", 443, "/repos/x/releases/latest")
        );
        let (_, host, port, path) = parse_url("http://127.0.0.1:8791/dl/a.zip").unwrap();
        assert_eq!(
            (host.as_str(), port, path.as_str()),
            ("127.0.0.1", 8791, "/dl/a.zip")
        );
        let (_, _, port, path) = parse_url("http://example.com").unwrap();
        assert_eq!((port, path.as_str()), (80, "/"));
        assert!(parse_url("example.com/no-scheme").is_err());
    }

    #[test]
    fn test_resolve_redirect() {
        let cur = "https://github.com/x/releases/download/v1/a.tar.gz";
        assert_eq!(
            resolve_redirect(cur, "https://objects.githubusercontent.com/abc"),
            "https://objects.githubusercontent.com/abc"
        );
        assert_eq!(
            resolve_redirect(cur, "//cdn.example.com/a"),
            "https://cdn.example.com/a"
        );
        assert_eq!(
            resolve_redirect(cur, "/other/path"),
            "https://github.com/other/path"
        );
        assert_eq!(
            resolve_redirect(cur, "sibling"),
            "https://github.com/x/releases/download/v1/sibling"
        );
    }

    #[test]
    fn test_parse_http_bytes_variants() {
        // Content-Length body.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let resp = parse_http_bytes(raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        // Chunked body.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let resp = parse_http_bytes(raw).unwrap();
        assert_eq!(resp.body, b"hello world");
        // Interim 100 response before the real one.
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let resp = parse_http_bytes(raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
        // Truncated chunk is an error, not silent data loss.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nshort\r\n";
        assert!(parse_http_bytes(raw).is_err());
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

    #[test]
    fn test_fetch_against_local_server() {
        let base = fake_server(vec![
            ("/plain".to_string(), http200(b"payload")),
            (
                "/old".to_string(),
                b"HTTP/1.1 302 Found\r\nLocation: /plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            (
                "/chunked".to_string(),
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n"
                    .to_vec(),
            ),
        ]);
        assert_eq!(fetch(&format!("{base}/plain")).unwrap(), b"payload");
        assert_eq!(fetch(&format!("{base}/old")).unwrap(), b"payload");
        assert_eq!(fetch(&format!("{base}/chunked")).unwrap(), b"Wikipedia");
        let err = fetch(&format!("{base}/missing")).unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    // ---------------- extraction & replacement ----------------

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pasture-update-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_find_extracted_binary() {
        let dir = unique_temp_dir("find");
        let pkg = dir.join("pasture-v1.0.0-linux-x86_64");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("README"), "x").unwrap();
        let bin = pkg.join("pasture");
        std::fs::write(&bin, "binary").unwrap();
        assert_eq!(find_extracted_binary(&dir), Some(bin));
        let empty = unique_temp_dir("findempty");
        assert_eq!(find_extracted_binary(&empty), None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_binary_swaps_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("replace");
        let exe = dir.join("pasture");
        std::fs::write(&exe, "old binary").unwrap();
        let new = dir.join("downloaded");
        std::fs::write(&new, "new binary").unwrap();
        let outcome = replace_binary(&exe, &new).unwrap();
        assert!(outcome.parked_old.is_none());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new binary");
        // The replacement is executable (the staged copy got mode 755).
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_binary_reported_version() {
        let dir = unique_temp_dir("version");
        let script = dir.join("pasture");
        std::fs::write(&script, "#!/bin/sh\necho \"pasture 9.9.9\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(binary_reported_version(&script).as_deref(), Some("9.9.9"));
        assert_eq!(binary_reported_version(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- full flow against a fake server ----------------

    /// Build a release archive the way release.yml does: a directory
    /// `pasture-<tag>-linux-x86_64/` holding an executable `pasture`.
    #[cfg(unix)]
    fn build_fixture_archive(dir: &Path, tag: &str, version: &str) -> (Vec<u8>, String) {
        use std::os::unix::fs::PermissionsExt;
        let name = format!("pasture-{tag}-linux-x86_64");
        let pkg = dir.join("src").join(&name);
        std::fs::create_dir_all(&pkg).unwrap();
        let script = pkg.join("pasture");
        std::fs::write(&script, format!("#!/bin/sh\necho \"pasture {version}\"\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join(format!("{name}.tar.gz"));
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.join("src"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(status.success(), "tar must be available for this test");
        let bytes = std::fs::read(&archive).unwrap();
        let hash = sha256_hex(&bytes);
        (bytes, hash)
    }

    #[cfg(unix)]
    fn installed_old_binary(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let install = dir.join("installed");
        std::fs::create_dir_all(&install).unwrap();
        let exe = install.join("pasture");
        std::fs::write(&exe, "#!/bin/sh\necho \"pasture 0.28.0\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    /// Serve a v9.9.9 release whose archive hash is `archive_hash` in the
    /// API digest, with an optional SHA256SUMS body.
    #[cfg(unix)]
    fn serve_release(archive: Vec<u8>, archive_hash: &str, sums: Option<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let archive_name = "pasture-v9.9.9-linux-x86_64.tar.gz";
        let mut assets = format!(
            r#"{{"name":"{archive_name}","browser_download_url":"{base}/dl/{archive_name}","digest":"sha256:{archive_hash}"}}"#
        );
        if sums.is_some() {
            assets.push_str(&format!(
                r#",{{"name":"SHA256SUMS","browser_download_url":"{base}/dl/SHA256SUMS"}}"#
            ));
        }
        let json = format!(r#"{{"tag_name":"v9.9.9","assets":[{assets}]}}"#);
        let mut routes = vec![
            ("/releases/latest".to_string(), http200(json.as_bytes())),
            (format!("/dl/{archive_name}"), http200(&archive)),
        ];
        if let Some(sums) = sums {
            routes.push(("/dl/SHA256SUMS".to_string(), http200(sums.as_bytes())));
        }
        spawn_server(listener, routes);
        base
    }

    #[cfg(unix)]
    #[test]
    fn test_perform_update_end_to_end() {
        let dir = unique_temp_dir("e2e");
        let (archive, hash) = build_fixture_archive(&dir, "v9.9.9", "9.9.9");
        let sums = format!("{hash}  pasture-v9.9.9-linux-x86_64.tar.gz\n");
        let base = serve_release(archive, &hash, Some(sums));
        let exe = installed_old_binary(&dir);
        let plan = UpdatePlan {
            api_base: base,
            current_version: "0.28.0".to_string(),
            exe_path: exe.clone(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            check_only: false,
        };
        match perform_update(&plan).unwrap() {
            UpdateOutcome::Updated {
                from,
                to,
                parked_old,
            } => {
                assert_eq!(from, "0.28.0");
                assert_eq!(to, "9.9.9");
                assert!(parked_old.is_none());
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        // The installed binary is the new one and runs as such.
        assert_eq!(binary_reported_version(&exe).as_deref(), Some("9.9.9"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_perform_update_check_only_and_up_to_date() {
        let dir = unique_temp_dir("checkonly");
        let (archive, hash) = build_fixture_archive(&dir, "v9.9.9", "9.9.9");
        let base = serve_release(archive, &hash, None);
        let exe = installed_old_binary(&dir);
        let before = std::fs::read(&exe).unwrap();
        let plan = UpdatePlan {
            api_base: base.clone(),
            current_version: "0.28.0".to_string(),
            exe_path: exe.clone(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            check_only: true,
        };
        match perform_update(&plan).unwrap() {
            UpdateOutcome::Available { latest } => assert_eq!(latest, "9.9.9"),
            other => panic!("expected Available, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            before,
            "--check must not install"
        );

        let plan = UpdatePlan {
            api_base: base,
            current_version: "9.9.9".to_string(),
            exe_path: exe.clone(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            check_only: false,
        };
        match perform_update(&plan).unwrap() {
            UpdateOutcome::UpToDate => {}
            other => panic!("expected UpToDate, got {other:?}"),
        }
        assert_eq!(std::fs::read(&exe).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_perform_update_checksum_mismatch_keeps_binary() {
        let dir = unique_temp_dir("badhash");
        let (archive, _real_hash) = build_fixture_archive(&dir, "v9.9.9", "9.9.9");
        // The API digest names a different archive than the one served.
        let wrong = sha256_hex(b"not the archive");
        let base = serve_release(archive, &wrong, None);
        let exe = installed_old_binary(&dir);
        let before = std::fs::read(&exe).unwrap();
        let plan = UpdatePlan {
            api_base: base,
            current_version: "0.28.0".to_string(),
            exe_path: exe.clone(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            check_only: false,
        };
        let err = perform_update(&plan).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            before,
            "a failed verification must leave the installed binary untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

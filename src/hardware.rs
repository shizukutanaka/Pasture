//! Hardware detection. Pure parsing functions are separated from the
//! environment-touching detection so they can be unit-tested offline.

use std::process::Command;

/// Detected GPU information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    pub vendor: String,
    /// Total VRAM in megabytes, when known.
    pub vram_mb: Option<u64>,
}

/// A snapshot of the host's relevant hardware capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareProfile {
    /// Total RAM in MB, or `None` when detection is unavailable on this OS
    /// (ADR-261). `None` must NOT collapse to 0: a 0 was previously read as
    /// "tiniest possible machine" and forced the CPU-only routing tier, so an
    /// undetected 64 GB Mac silently shipped everything to the paid cloud.
    pub ram_mb: Option<u64>,
    pub cpu_count: usize,
    pub gpu: Option<GpuInfo>,
}

impl HardwareProfile {
    /// Detect the current host profile. Best-effort: any probe that fails leaves
    /// the field `None` rather than panicking or inventing a value (US-1).
    pub fn detect() -> Self {
        let ram_mb = detect_ram_mb();
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let gpu = detect_gpu();
        Self {
            ram_mb,
            cpu_count,
            gpu,
        }
    }

    /// True when a GPU with at least `min_vram_mb` of VRAM is present.
    pub fn has_capable_gpu(&self, min_vram_mb: u64) -> bool {
        match &self.gpu {
            Some(g) => g.vram_mb.map(|v| v >= min_vram_mb).unwrap_or(true),
            None => false,
        }
    }
}

/// Parse `MemTotal` (in kB) out of `/proc/meminfo` contents, returning MB.
pub fn parse_meminfo(contents: &str) -> Option<u64> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Parse the total VRAM (MB) from `nvidia-smi --query-gpu=memory.total`
/// CSV output (e.g. `"8192 MiB\n"` or `"8192\n"`).
pub fn parse_nvidia_vram(output: &str) -> Option<u64> {
    output
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(|t| t.parse().ok())
}

/// Parse `sysctl -n hw.memsize` output (macOS): total RAM **in bytes** on a
/// single line, e.g. `"68719476736\n"`. Returns MB. Non-numeric → `None`.
pub fn parse_sysctl_memsize(output: &str) -> Option<u64> {
    let bytes: u64 = output.split_whitespace().next()?.parse().ok()?;
    Some(bytes / 1024 / 1024)
}

/// Parse `wmic ComputerSystem get TotalPhysicalMemory` output (Windows): a header
/// line `TotalPhysicalMemory` followed by the value **in bytes**, plus trailing
/// blank lines (`\r\n`), e.g. `"TotalPhysicalMemory\n17179869184\n\n"`. Returns
/// MB from the first all-digit token found; anything else → `None`.
pub fn parse_wmic_meminfo(output: &str) -> Option<u64> {
    for line in output.lines() {
        let t = line.trim();
        if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
            let bytes: u64 = t.parse().ok()?;
            return Some(bytes / 1024 / 1024);
        }
    }
    None
}

/// Detect total RAM in MB across OSes (ADR-261), first hit wins:
/// 1. `PASTURE_RAM_MB` override (mirrors `PASTURE_GPU_VRAM_MB`) — lets a user on
///    an exotic platform set it by hand, and makes the path testable anywhere;
/// 2. Linux `/proc/meminfo`;
/// 3. macOS `sysctl -n hw.memsize`;
/// 4. Windows `wmic ComputerSystem get TotalPhysicalMemory` (on Windows builds
///    that dropped `wmic`, `PASTURE_RAM_MB` or PowerShell
///    `Get-CimInstance Win32_ComputerSystem` is the documented fallback).
///
/// Returns `None` only when every probe fails — a real, representable "unknown".
fn detect_ram_mb() -> Option<u64> {
    if let Ok(v) = std::env::var("PASTURE_RAM_MB") {
        if let Ok(mb) = v.trim().parse::<u64>() {
            return Some(mb);
        }
    }
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(mb) = parse_meminfo(&contents) {
            return Some(mb);
        }
    }
    if let Some(mb) = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_sysctl_memsize(&String::from_utf8_lossy(&o.stdout)))
    {
        return Some(mb);
    }
    Command::new("wmic")
        .args(["ComputerSystem", "get", "TotalPhysicalMemory"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_wmic_meminfo(&String::from_utf8_lossy(&o.stdout)))
}

fn detect_gpu() -> Option<GpuInfo> {
    // Explicit override for testing / headless control.
    if let Ok(v) = std::env::var("PASTURE_GPU_VRAM_MB") {
        if let Ok(mb) = v.parse::<u64>() {
            return Some(GpuInfo {
                vendor: "override".to_string(),
                vram_mb: Some(mb),
            });
        }
    }
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Some(GpuInfo {
        vendor: "nvidia".to_string(),
        vram_mb: parse_nvidia_vram(&stdout),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meminfo_valid_returns_mb() {
        let sample = "MemTotal:       16384000 kB\nMemFree: 1000 kB\n";
        assert_eq!(parse_meminfo(sample), Some(16000));
    }

    #[test]
    fn test_parse_meminfo_missing_returns_none() {
        assert_eq!(parse_meminfo("SwapTotal: 0 kB\n"), None);
    }

    #[test]
    fn test_parse_meminfo_malformed_returns_none() {
        assert_eq!(parse_meminfo("MemTotal:   notanumber kB"), None);
    }

    #[test]
    fn test_parse_nvidia_vram_with_units() {
        assert_eq!(parse_nvidia_vram("8192 MiB\n"), Some(8192));
    }

    #[test]
    fn test_parse_nvidia_vram_plain() {
        assert_eq!(parse_nvidia_vram("24576\n"), Some(24576));
    }

    #[test]
    fn test_parse_nvidia_vram_empty_returns_none() {
        assert_eq!(parse_nvidia_vram(""), None);
    }

    #[test]
    fn test_has_capable_gpu_threshold() {
        let p = HardwareProfile {
            ram_mb: Some(16000),
            cpu_count: 8,
            gpu: Some(GpuInfo {
                vendor: "nvidia".into(),
                vram_mb: Some(8192),
            }),
        };
        assert!(p.has_capable_gpu(8000));
        assert!(!p.has_capable_gpu(12000));
    }

    #[test]
    fn test_has_capable_gpu_none() {
        let p = HardwareProfile {
            ram_mb: Some(8000),
            cpu_count: 4,
            gpu: None,
        };
        assert!(!p.has_capable_gpu(1));
    }

    #[test]
    fn test_parse_sysctl_memsize() {
        // macOS reports total RAM in bytes. 64 GiB → 65536 MB.
        assert_eq!(parse_sysctl_memsize("68719476736\n"), Some(65536));
        assert_eq!(parse_sysctl_memsize("17179869184"), Some(16384));
        assert_eq!(parse_sysctl_memsize(""), None);
        assert_eq!(parse_sysctl_memsize("not a number"), None);
    }

    #[test]
    fn test_parse_wmic_meminfo() {
        // Windows `wmic` prints a header then the value, with trailing CRLF blanks.
        assert_eq!(
            parse_wmic_meminfo("TotalPhysicalMemory\r\n17179869184\r\n\r\n"),
            Some(16384)
        );
        assert_eq!(
            parse_wmic_meminfo("TotalPhysicalMemory\n68719476736\n"),
            Some(65536)
        );
        assert_eq!(parse_wmic_meminfo("TotalPhysicalMemory\n\n"), None);
        assert_eq!(parse_wmic_meminfo(""), None);
    }
}

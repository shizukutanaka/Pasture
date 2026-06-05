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
    pub ram_mb: u64,
    pub cpu_count: usize,
    pub gpu: Option<GpuInfo>,
}

impl HardwareProfile {
    /// Detect the current host profile. Best-effort: any probe that fails
    /// degrades to a conservative value rather than panicking (US-1).
    pub fn detect() -> Self {
        let ram_mb = detect_ram_mb().unwrap_or(0);
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

fn detect_ram_mb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&contents)
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
            ram_mb: 16000,
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
            ram_mb: 8000,
            cpu_count: 4,
            gpu: None,
        };
        assert!(!p.has_capable_gpu(1));
    }
}

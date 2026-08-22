//! Portable host telemetry with an honest legacy-wire boundary.
//!
//! `SystemSnapshot` has required macOS-era fields (including compressor and agent
//! RSS) and cannot honestly represent partial portable data.  This adapter keeps
//! portable measurements typed until a later additive wire shape is introduced.

use std::{collections::VecDeque, ffi::OsStr, path::Path};

use agentdeck_core::{
    CapabilityBackend, CapabilityLevel, CapabilityReason, CapabilityState, CapabilityStatus,
    HostFeed,
};
use sysinfo::{Process, ProcessesToUpdate, System};

use crate::config::HostTelemetryMode;

use super::capability;

const HISTORY_LIMIT: usize = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct BasicHostSnapshot {
    pub cpu_busy: Option<f64>,
    pub cpu_history: Vec<i64>,
    pub ram_used_gb: Option<f64>,
    pub ram_total_gb: Option<f64>,
    pub swap_used_gb: Option<f64>,
    pub swap_total_gb: Option<f64>,
    pub cores: i64,
    pub agent_processes: Option<AgentProcessTotals>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentProcessTotals {
    pub count: i64,
    /// Process CPU is delta-derived too, so the first enumeration has no honest value.
    pub cpu: Option<f64>,
    pub rss_gb: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostOutcome {
    pub capability: CapabilityStatus,
    pub feed: HostFeed,
    pub basic: Option<BasicHostSnapshot>,
}

#[derive(Debug)]
pub struct HostSampler {
    system: System,
    history: VecDeque<i64>,
    cpu_warmed: bool,
    processes_warmed: bool,
}

impl Default for HostSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl HostSampler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            system: System::new(),
            history: VecDeque::with_capacity(HISTORY_LIMIT),
            cpu_warmed: false,
            processes_warmed: false,
        }
    }

    /// CPU use is delta-based in sysinfo. The first sample is deliberately a warm-up:
    /// `cpu_busy` is absent rather than a misleading 0%, while memory/load/core values
    /// remain available.
    pub fn sample(&mut self, mode: HostTelemetryMode) -> HostOutcome {
        if mode == HostTelemetryMode::Off {
            return HostOutcome {
                capability: capability(
                    CapabilityState::Disabled,
                    Some(CapabilityBackend::System),
                    Some(CapabilityReason::ProviderDisabled),
                    None,
                ),
                feed: empty_feed(),
                basic: None,
            };
        }
        if mode == HostTelemetryMode::Detailed && !detailed_supported() {
            return HostOutcome {
                capability: capability(
                    CapabilityState::Unsupported,
                    Some(CapabilityBackend::Native),
                    Some(CapabilityReason::Unsupported),
                    None,
                ),
                feed: empty_feed(),
                basic: None,
            };
        }

        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        let cpu = self.system.global_cpu_usage();
        let cpu_busy = optional_cpu_busy(self.cpu_warmed, cpu);
        self.cpu_warmed = true;
        if let Some(cpu_busy) = cpu_busy {
            self.history.push_back(cpu_busy.round() as i64);
            while self.history.len() > HISTORY_LIMIT {
                let _ = self.history.pop_front();
            }
        }
        let load = System::load_average();
        let logical_cores = self.system.cpus().len();
        let cores = (logical_cores > 0)
            .then_some(logical_cores)
            .or_else(|| {
                std::thread::available_parallelism()
                    .ok()
                    .map(|count| count.get())
            })
            .and_then(|count| i64::try_from(count).ok())
            .unwrap_or_default();
        let ram_total = self.system.total_memory();
        let swap_total = self.system.total_swap();
        let agent_processes =
            agent_process_totals(self.system.processes().values(), self.processes_warmed);
        self.processes_warmed = true;
        let basic = BasicHostSnapshot {
            cpu_busy,
            cpu_history: self.history.iter().copied().collect(),
            ram_used_gb: known_bytes(self.system.used_memory(), ram_total),
            ram_total_gb: supported_total_bytes(ram_total),
            swap_used_gb: known_bytes(self.system.used_swap(), swap_total),
            swap_total_gb: supported_total_bytes(swap_total),
            cores,
            agent_processes: Some(agent_processes),
        };
        HostOutcome {
            capability: CapabilityStatus {
                level: Some(CapabilityLevel::Basic),
                ..capability(
                    CapabilityState::Available,
                    Some(CapabilityBackend::System),
                    None,
                    None,
                )
            },
            feed: HostFeed {
                ok: cores > 0,
                load1: round_1(load.one),
                load5: round_1(load.five),
                cores,
                // Do not put a partial portable sample into the all-required detailed
                // wire shape. It would fabricate absent metrics as zero.
                system: None,
            },
            basic: Some(basic),
        }
    }
}

fn empty_feed() -> HostFeed {
    HostFeed {
        ok: false,
        load1: 0.0,
        load5: 0.0,
        cores: 0,
        system: None,
    }
}

#[must_use]
pub fn matches_agent_process(command: &Path) -> bool {
    command.file_name().is_some_and(is_agent_name)
}

/// `sysinfo` may have only a process name, only an executable path, or both. Wrapper
/// commands are accepted only when the wrapped executable is an exact known agent name;
/// a path such as `claude-notes` therefore can never inflate the totals.
pub fn matches_agent_process_parts(
    executable: Option<&Path>,
    name: &OsStr,
    command: &[std::ffi::OsString],
) -> bool {
    executable.is_some_and(matches_agent_process)
        || is_agent_name(name)
        || is_known_wrapper(name)
            && command
                .iter()
                .skip(1)
                .find(|part| !part.to_string_lossy().starts_with('-'))
                .is_some_and(|part| is_agent_name(Path::new(part).file_name().unwrap_or(part)))
}

fn is_agent_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(
        name,
        "claude" | "codex" | "ollama" | "pi" | "herdr" | "copilot"
    ) || name.starts_with("copilot-")
}

fn is_known_wrapper(name: &OsStr) -> bool {
    matches!(
        name.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("env" | "node" | "bun" | "python" | "python3")
    )
}

fn agent_process_totals<'a>(
    processes: impl Iterator<Item = &'a Process>,
    warmed: bool,
) -> AgentProcessTotals {
    let mut count = 0_i64;
    let mut cpu = 0.0;
    let mut rss = 0_u64;
    for process in processes
        .filter(|process| matches_agent_process_parts(process.exe(), process.name(), process.cmd()))
    {
        count += 1;
        cpu += f64::from(process.cpu_usage());
        rss = rss.saturating_add(process.memory());
    }
    AgentProcessTotals {
        count,
        cpu: warmed.then(|| round_1(cpu)),
        rss_gb: Some(round_1(rss as f64 / 1_073_741_824.0)),
    }
}

fn detailed_supported() -> bool {
    // Detailed legacy fields require native probes. They intentionally remain absent
    // until each platform adapter can measure them; sysinfo's portable values are basic.
    false
}

fn supported_total_bytes(bytes: u64) -> Option<f64> {
    (bytes > 0).then(|| round_1(bytes as f64 / 1_073_741_824.0))
}

fn known_bytes(bytes: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| round_1(bytes as f64 / 1_073_741_824.0))
}

fn round_1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn optional_cpu_busy(warmed: bool, cpu: f32) -> Option<f64> {
    (warmed && cpu.is_finite()).then(|| round_1(f64::from(cpu)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn matches_known_agent_executables_without_path_substring_false_positives() {
        for path in [
            "/usr/local/bin/claude",
            "codex",
            "C:/tools/copilot.exe",
            "pi",
            "herdr",
        ] {
            assert!(matches_agent_process(Path::new(path)));
        }
        assert!(!matches_agent_process(Path::new("/tmp/claude-notes")));
    }

    #[test]
    fn off_and_detailed_never_fabricate_a_system_snapshot() {
        let mut sampler = HostSampler::new();
        let off = sampler.sample(HostTelemetryMode::Off);
        assert_eq!(off.capability.state, CapabilityState::Disabled);
        assert!(off.basic.is_none());
        let detailed = sampler.sample(HostTelemetryMode::Detailed);
        assert_eq!(detailed.capability.state, CapabilityState::Unsupported);
        assert!(detailed.feed.system.is_none());
    }

    #[test]
    fn basic_reuses_one_sampler_and_bounds_history() {
        let mut sampler = HostSampler::new();
        for _ in 0..(HISTORY_LIMIT + 4) {
            let result = sampler.sample(HostTelemetryMode::Basic);
            assert_eq!(result.capability.level, Some(CapabilityLevel::Basic));
            assert!(result.feed.system.is_none());
        }
        assert!(sampler.history.len() <= HISTORY_LIMIT);
    }

    #[test]
    fn zero_bytes_are_distinguished_from_unsupported_totals() {
        assert_eq!(known_bytes(0, 1_073_741_824), Some(0.0));
        assert_eq!(known_bytes(0, 0), None);
        assert_eq!(supported_total_bytes(0), None);
    }

    #[test]
    fn only_the_first_cpu_sample_is_a_warmup() {
        assert_eq!(optional_cpu_busy(false, 0.0), None);
        assert_eq!(optional_cpu_busy(true, 0.0), Some(0.0));
    }

    #[test]
    fn matching_is_exact_but_accepts_known_wrappers_and_copilot() {
        use std::ffi::OsString;
        assert!(matches_agent_process_parts(
            None,
            OsStr::new("env"),
            &[OsString::from("env"), OsString::from("claude")],
        ));
        assert!(matches_agent_process_parts(
            None,
            OsStr::new("copilot.exe"),
            &[]
        ));
        assert!(!matches_agent_process_parts(
            None,
            OsStr::new("env"),
            &[OsString::from("env"), OsString::from("claude-notes")],
        ));
    }
}

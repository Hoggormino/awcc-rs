//! CPUFreq policy ownership. No driver-mode changes, command-line edits or power limits.
use awcc_core::PowerProfile;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CpuPolicyOutcome {
    pub confirmed: bool,
    pub controls: Vec<String>,
    pub errors: Vec<String>,
}
pub trait CpuPolicyBackend: Send {
    fn available(&self, profile: PowerProfile) -> bool;
    fn apply(&mut self, profile: PowerProfile, current: &dyn Fn() -> bool) -> CpuPolicyOutcome;
}
/// Deterministic backend for offline thermal/coordinator tests.
#[derive(Default)]
pub struct SimCpuPolicy;
impl CpuPolicyBackend for SimCpuPolicy {
    fn available(&self, _: PowerProfile) -> bool {
        true
    }
    fn apply(&mut self, _: PowerProfile, current: &dyn Fn() -> bool) -> CpuPolicyOutcome {
        CpuPolicyOutcome {
            confirmed: current(),
            controls: vec!["simulated CPU policy".into()],
            errors: vec![],
        }
    }
}
pub struct LinuxCpuPolicy {
    root: PathBuf,
}
impl LinuxCpuPolicy {
    pub fn system() -> Self {
        Self::new("/sys/devices/system/cpu/cpufreq")
    }
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn plan(&self, profile: PowerProfile) -> Result<Vec<(PathBuf, String)>, String> {
        let entries = fs::read_dir(&self.root).map_err(|e| format!("CPUFreq unavailable: {e}"))?;
        let mut policies = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("policy"))
            })
            .collect::<Vec<_>>();
        policies.sort();
        if policies.is_empty() {
            return Err("no CPUFreq policies available".into());
        }
        let mut plan = Vec::new();
        for policy in policies {
            let driver = read(&policy.join("scaling_driver"))?;
            let governors = read(&policy.join("scaling_available_governors"))?;
            let epp = policy.join("energy_performance_preference");
            // Active HWP/EPP drivers need powersave to permit EPP writes. Passive drivers use
            // the governors they actually advertise, including Intel/AMD fallback modes.
            let active_epp =
                epp.exists() && matches!(driver.as_str(), "intel_pstate" | "amd-pstate-epp");
            let candidates: &[&str] = if active_epp {
                &["powersave"]
            } else {
                match profile {
                    PowerProfile::PowerSaver => {
                        &["powersave", "conservative", "schedutil", "ondemand"]
                    }
                    PowerProfile::Balanced => {
                        &["schedutil", "ondemand", "conservative", "powersave"]
                    }
                    PowerProfile::Performance => &["performance"],
                }
            };
            let governor = choose(&governors, candidates).ok_or_else(|| {
                format!(
                    "{}: no suitable governor for {}",
                    policy.display(),
                    profile.as_str()
                )
            })?;
            plan.push((policy.join("scaling_governor"), governor.into()));
            if active_epp {
                let available = read(&policy.join("energy_performance_available_preferences"))?;
                let choices: &[&str] = match profile {
                    PowerProfile::PowerSaver => &["power", "balance_power"],
                    PowerProfile::Balanced => &["balance_performance", "balance_power", "default"],
                    PowerProfile::Performance => &["performance"],
                };
                let value = choose(&available, choices).ok_or_else(|| {
                    format!(
                        "{}: no suitable EPP for {}",
                        policy.display(),
                        profile.as_str()
                    )
                })?;
                plan.push((epp, value.into()));
            }
        }
        Ok(plan)
    }
}
fn choose<'a>(available: &str, choices: &[&'a str]) -> Option<&'a str> {
    choices
        .iter()
        .copied()
        .find(|c| available.split_whitespace().any(|a| a == *c))
}
fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| format!("{}: {e}", path.display()))
}
impl CpuPolicyBackend for LinuxCpuPolicy {
    fn available(&self, profile: PowerProfile) -> bool {
        self.plan(profile).is_ok()
    }
    fn apply(&mut self, profile: PowerProfile, current: &dyn Fn() -> bool) -> CpuPolicyOutcome {
        let mut outcome = CpuPolicyOutcome::default();
        let plan = match self.plan(profile) {
            Ok(p) => p,
            Err(e) => {
                outcome.errors.push(e);
                return outcome;
            }
        };
        for (path, value) in plan {
            if !current() {
                outcome
                    .errors
                    .push("superseded by a newer selection".into());
                return outcome;
            }
            match read(&path).and_then(|old| {
                if old != value {
                    fs::write(&path, &value).map_err(|e| format!("{}: {e}", path.display()))?;
                }
                let observed = read(&path)?;
                if observed != value {
                    return Err(format!(
                        "{}: requested {value}, observed {observed}",
                        path.display()
                    ));
                }
                Ok(())
            }) {
                Ok(()) => outcome.controls.push(format!("{}={value}", path.display())),
                Err(e) => {
                    outcome.errors.push(e);
                    return outcome;
                }
            }
        }
        outcome.confirmed = true;
        outcome
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    fn fixture(tag: &str, driver: &str, governors: &str, epp: Option<&str>) -> PathBuf {
        let root = std::env::temp_dir().join(format!("awcc-cpu-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let p = root.join("policy0");
        fs::create_dir_all(&p).unwrap();
        for (name, value) in [
            ("scaling_driver", driver),
            ("scaling_available_governors", governors),
            ("scaling_governor", "powersave"),
        ] {
            fs::write(p.join(name), value).unwrap();
        }
        if let Some(epp) = epp {
            fs::write(p.join("energy_performance_available_preferences"), epp).unwrap();
            fs::write(p.join("energy_performance_preference"), "default").unwrap();
        }
        root
    }
    #[test]
    fn active_intel_and_amd_use_supported_epp_without_turbo_writes() {
        for driver in ["intel_pstate", "amd-pstate-epp"] {
            let root = fixture(
                driver,
                driver,
                "powersave performance",
                Some("power balance_power balance_performance performance"),
            );
            let mut cpu = LinuxCpuPolicy::new(&root);
            for p in PowerProfile::ALL {
                assert!(cpu.available(p));
                assert!(cpu.apply(p, &|| true).confirmed);
            }
            assert_eq!(
                read(&root.join("policy0/energy_performance_preference")).unwrap(),
                "performance"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn ignored_epp_write_is_a_partial_failure_not_confirmation() {
        let root = fixture(
            "ignored-epp",
            "amd-pstate-epp",
            "powersave performance",
            Some("power balance_power balance_performance performance"),
        );
        let epp = root.join("policy0/energy_performance_preference");
        fs::remove_file(&epp).unwrap();
        std::os::unix::fs::symlink("/dev/null", &epp).unwrap();
        let outcome = LinuxCpuPolicy::new(&root).apply(PowerProfile::Performance, &|| true);
        assert!(!outcome.confirmed);
        assert!(outcome.errors[0].contains("requested performance"));
        assert!(!outcome.controls.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn passive_and_generic_fallbacks_and_missing_controls() {
        for driver in ["intel_cpufreq", "amd-pstate", "acpi-cpufreq"] {
            let root = fixture(driver, driver, "powersave schedutil performance", None);
            let mut cpu = LinuxCpuPolicy::new(&root);
            for p in PowerProfile::ALL {
                assert!(cpu.apply(p, &|| true).confirmed);
            }
            fs::remove_dir_all(root).unwrap();
        }
        let root = fixture("missing", "amd-pstate-epp", "powersave", Some("default"));
        let mut cpu = LinuxCpuPolicy::new(&root);
        assert!(!cpu.available(PowerProfile::Performance));
        assert!(!cpu.apply(PowerProfile::Performance, &|| true).confirmed);
        assert!(!cpu.apply(PowerProfile::Balanced, &|| false).confirmed);
        fs::remove_dir_all(root).unwrap();
    }
}

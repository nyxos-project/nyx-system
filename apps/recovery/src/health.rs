//! Health: how full the disks are, what failed, what the last boot complained about, and
//! making room.

use crate::paths::Paths;
use crate::run::{Log, quiet, run};
use anyhow::{Result, ensure};
use std::ffi::CString;

#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    pub mount: String,
    pub total: u64,
    pub free: u64,
}

impl Space {
    pub fn used_percent(&self) -> u64 {
        ((self.total - self.free) * 100).checked_div(self.total).unwrap_or(0)
    }

    /// Nearly full: what breaks rebuilds and boot-menu updates first.
    pub fn low(&self) -> bool {
        self.used_percent() >= 90
            || self.free < 512 * 1024 * 1024 && self.mount != "/boot"
            || self.mount == "/boot" && self.free < 32 * 1024 * 1024
    }
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let size = s.f_frsize as u64;
    Some((s.f_fsid as u64, s.f_blocks as u64 * size, s.f_bavail as u64 * size))
}

/// /, /boot, /nix and /home, each filesystem once.
pub fn spaces() -> Vec<Space> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for m in ["/", "/boot", "/nix", "/home"] {
        if let Some((id, total, free)) = statvfs(m)
            && !seen.contains(&id)
        {
            seen.push(id);
            out.push(Space { mount: m.into(), total, free });
        }
    }
    out
}

pub fn size(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

/// Units that failed, from `systemctl list-units --failed --plain --no-legend`.
pub fn parse_failed(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split_whitespace().next()).filter(|u| u.contains('.')).map(String::from).collect()
}

pub fn failed_units() -> Vec<String> {
    quiet(&["systemctl", "list-units", "--failed", "--plain", "--no-legend", "--no-pager"])
        .map(|t| parse_failed(&t))
        .unwrap_or_default()
}

/// The previous boot's errors (the one that went wrong, when recovery was chosen after it).
pub fn last_boot_errors() -> String {
    match quiet(&["journalctl", "--boot=-1", "--priority=err", "--no-pager", "--output=short", "--lines=300"]) {
        Ok(t) if t.trim().is_empty() || t.contains("-- No entries --") => "The last boot logged no errors.".into(),
        Ok(t) => t,
        Err(e) => format!("The last boot's log can't be read: {e:#}"),
    }
}

/// Remove generations older than `days` (never the default), then what nothing needs,
/// then refresh the boot menu (which is what frees /boot).
pub fn clean(p: &Paths, days: u32, log: &Log) -> Result<String> {
    ensure!((1..=3650).contains(&days), "keep between a day and ten years");
    let profile = p.profile().display().to_string();
    run(log, &["nix-env", "--profile", &profile, "--delete-generations", &format!("{days}d")], None)?;
    let out = run(log, &["nix-store", "--gc"], None)?;
    let switch = p.profile().join("bin/switch-to-configuration").display().to_string();
    run(log, &[&switch, "boot"], None)?;
    Ok(out.lines().rev().find(|l| l.contains("freed")).map_or("Cleaned up".into(), |l| l.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_units_from_systemctl() {
        let t = "nyx-system-manager.service loaded failed failed The Nyx System Manager\nhome.mount loaded failed failed /home\n";
        assert_eq!(parse_failed(t), vec!["nyx-system-manager.service", "home.mount"]);
        assert!(parse_failed("").is_empty());
    }

    #[test]
    fn low_space() {
        let gb = 1_000_000_000;
        assert!(Space { mount: "/".into(), total: 100 * gb, free: 5 * gb }.low());
        assert!(!Space { mount: "/".into(), total: 100 * gb, free: 40 * gb }.low());
        assert!(Space { mount: "/boot".into(), total: 512 * 1_000_000, free: 20 * 1_000_000 }.low());
        assert!(!Space { mount: "/boot".into(), total: 1024 * 1_000_000, free: 400 * 1_000_000 }.low());
        assert_eq!(size(5 * gb), "5.0 GB");
        assert_eq!(size(300_000_000), "300 MB");
    }

    #[test]
    fn this_machine_has_a_root() {
        assert!(spaces().iter().any(|s| s.mount == "/" && s.total > 0));
    }
}

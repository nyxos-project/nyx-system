//! The system profile's generations, from its links.

use anyhow::Result;
use nyx_system_proto::Generation;
use std::path::Path;

const PROFILES: &str = "/nix/var/nix/profiles";

/// `system-42-link` → 42.
fn number(name: &str) -> Option<u32> {
    name.strip_prefix("system-")?.strip_suffix("-link")?.parse().ok()
}

/// The generation the system profile points at.
pub fn current() -> Option<u32> {
    std::fs::read_link(Path::new(PROFILES).join("system")).ok()?.to_str().and_then(number)
}

/// Newest first.
/// A generation's NyxOS version (its os-release's `VERSION_ID`); NixOS's (`nixos-version`)
/// for generations from before NyxOS had one.
fn version_of(generation: &std::path::Path) -> String {
    std::fs::read_to_string(generation.join("etc/os-release"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("VERSION_ID=")).map(|v| v.trim_matches('"').to_string()))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::fs::read_to_string(generation.join("nixos-version")).unwrap_or_default().trim().to_string())
}

pub fn list() -> Result<Vec<Generation>> {
    let current = current();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(PROFILES)? {
        let entry = entry?;
        let Some(n) = entry.file_name().to_str().and_then(number) else { continue };
        let created = entry
            .path()
            .symlink_metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let version = version_of(&entry.path());
        out.push(Generation { number: n, created, version, current: Some(n) == current });
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.number));
    Ok(out)
}

/// Drop the settings saved for generations that are gone (`STATE/generations/N.json`).
pub fn forget_missing(state: &Path) {
    let kept: Vec<u32> = list().map(|l| l.into_iter().map(|g| g.number).collect()).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(state.join("generations")) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_suffix(".json").and_then(|n| n.parse::<u32>().ok())
            && !kept.contains(&n)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_names() {
        assert_eq!(super::number("system-42-link"), Some(42));
        assert_eq!(super::number("system"), None);
        assert_eq!(super::number("default-3-link"), None);
    }
}

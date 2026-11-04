//! Generations and configuration repair. Nothing here switches the running system: a
//! generation is made the one that boots, with the settings and lock it was built from
//! put back, so the next change made in Settings builds on it.

use crate::paths::Paths;
use crate::run::{Log, run};
use anyhow::{Context, Result, ensure};
use nyx_system_proto::SystemSettings;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Generation {
    pub number: u32,
    /// Seconds since the epoch.
    pub created: i64,
    pub version: String,
    /// The one that boots by default.
    pub default: bool,
    /// The one running now (recovery is one of its boot entries).
    pub running: bool,
    /// Its settings layer is known, so going back to it can put them back.
    pub settings: bool,
}

fn number(name: &str) -> Option<u32> {
    name.strip_prefix("system-")?.strip_suffix("-link")?.parse().ok()
}

pub fn default_generation(p: &Paths) -> Option<u32> {
    std::fs::read_link(p.profile()).ok()?.file_name()?.to_str().and_then(number)
}

/// The store path a link resolves to, to tell which generation is running.
fn target(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Newest first. `booted` is the running system (`/run/booted-system`).
pub fn list(p: &Paths, booted: &Path) -> Result<Vec<Generation>> {
    let default = default_generation(p);
    let running = target(booted);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&p.profiles).with_context(|| format!("can't read {}", p.profiles.display()))? {
        let entry = entry?;
        let Some(n) = entry.file_name().to_str().and_then(number) else { continue };
        let created = entry
            .path()
            .symlink_metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        let version = std::fs::read_to_string(entry.path().join("nixos-version")).unwrap_or_default().trim().to_string();
        let here = target(&entry.path());
        out.push(Generation {
            number: n,
            created,
            version,
            default: Some(n) == default,
            running: here.is_some() && here == running,
            settings: settings_of(p, n).is_some(),
        });
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.number));
    Ok(out)
}

/// The settings layer generation `n` was built from: carried in the generation itself,
/// else the copy the System Manager kept.
pub fn settings_of(p: &Paths, n: u32) -> Option<String> {
    [p.generation(n).join("etc/nyxos/built-from/system.json"), p.state.join("generations").join(format!("{n}.json"))]
        .iter()
        .find_map(|f| std::fs::read_to_string(f).ok())
        .filter(|t| SystemSettings::parse(t).is_ok())
}

/// The flake.lock generation `n` was built from, when it carries one.
pub fn lock_of(p: &Paths, n: u32) -> Option<Vec<u8>> {
    std::fs::read(p.generation(n).join("etc/nyxos/built-from/flake.lock")).ok()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("recovery-new");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Put back generation `n`'s settings and lock, keeping the current ones beside them.
/// What was put back, in words.
pub fn restore_inputs(p: &Paths, n: u32) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if let Some(text) = settings_of(p, n) {
        let file = p.settings_file();
        if let Ok(old) = std::fs::read(&file) {
            write_atomic(&file.with_extension("json.before-recovery"), &old)?;
        }
        write_atomic(&file, text.as_bytes())?;
        done.push(format!("the system settings generation {n} was built from"));
    }
    if let Some(lock) = lock_of(p, n) {
        let file = p.flake.join("flake.lock");
        if let Ok(old) = std::fs::read(&file) {
            write_atomic(&file.with_extension("lock.before-recovery"), &old)?;
        }
        write_atomic(&file, &lock)?;
        done.push(format!("the versions (flake.lock) generation {n} was built with"));
    }
    Ok(done)
}

/// Make generation `n` the one that boots, and put back what it was built from.
pub fn make_default(p: &Paths, n: u32, log: &Log) -> Result<String> {
    ensure!(p.generation(n).exists(), "generation {n} is gone");
    let profile = p.profile().display().to_string();
    run(log, &["nix-env", "--profile", &profile, "--switch-generation", &n.to_string()], None)?;
    let switch = p.profile().join("bin/switch-to-configuration").display().to_string();
    run(log, &[&switch, "boot"], None)?;
    let restored = restore_inputs(p, n)?;
    let mut msg = format!("Generation {n} boots from now on. Restart to use it.");
    if !restored.is_empty() {
        msg.push_str(&format!(" Put back {}.", restored.join(" and ")));
    }
    Ok(msg)
}

/// The generation before the default one, for undoing the last system change.
pub fn previous(list: &[Generation]) -> Option<u32> {
    let default = list.iter().find(|g| g.default)?.number;
    list.iter().map(|g| g.number).filter(|&n| n < default).max()
}

/// The settings layer now, pretty.
pub fn settings_now(p: &Paths) -> String {
    match std::fs::read_to_string(p.settings_file()) {
        Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(t),
            Err(e) => format!("This file is damaged ({e}):\n\n{t}"),
        },
        Err(_) => "No system settings have been changed; the distribution's and this machine's own apply.".into(),
    }
}

/// Build the configuration from what's on disk, without the network, and make it the one
/// that boots.
pub fn rebuild_offline(p: &Paths, log: &Log) -> Result<String> {
    let flake = format!("path:{}#{}", p.flake.display(), p.host);
    let overrides = format!("path:{}", p.overrides.display());
    run(
        log,
        &[
            "nixos-rebuild",
            "boot",
            "--option",
            "substitute",
            "false",
            "--flake",
            &flake,
            "--override-input",
            "nyxos-overrides",
            &overrides,
        ],
        None,
    )?;
    Ok("The rebuilt system boots from now on. Restart to use it.".into())
}

/// Reset the settings layer to none (the distribution's and this machine's own settings),
/// keeping the old file beside it, then rebuild.
pub fn reset_settings(p: &Paths, log: &Log) -> Result<String> {
    let file = p.settings_file();
    if let Ok(old) = std::fs::read(&file) {
        write_atomic(&file.with_extension("json.before-recovery"), &old)?;
    }
    write_atomic(&file, b"{\n  \"version\": 1\n}\n")?;
    let _ = log.send(format!("The old settings are kept in {}", file.with_extension("json.before-recovery").display()));
    rebuild_offline(p, log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        for n in [40, 41, 42] {
            let g = root.join(format!("store/gen{n}"));
            std::fs::create_dir_all(g.join("etc/nyxos/built-from")).unwrap();
            std::fs::write(g.join("nixos-version"), format!("26.05.{n}\n")).unwrap();
            symlink(&g, root.join(format!("profiles/system-{n}-link"))).unwrap();
        }
        std::fs::write(root.join("store/gen41/etc/nyxos/built-from/system.json"), "{\"version\":1,\"hostname\":\"forty-one\"}")
            .unwrap();
        std::fs::write(root.join("store/gen41/etc/nyxos/built-from/flake.lock"), "lock41").unwrap();
        std::fs::create_dir_all(root.join("state/generations")).unwrap();
        std::fs::write(root.join("state/generations/40.json"), "{\"version\":1,\"hostname\":\"forty\"}").unwrap();
        symlink("system-42-link", root.join("profiles/system")).unwrap();
        let p = Paths {
            profiles: root.join("profiles"),
            state: root.join("state"),
            overrides: root.join("overrides"),
            flake: root.join("flake"),
            host: "test".into(),
            passwd: root.join("passwd"),
            shadow: root.join("shadow"),
            mounts: root.join("mnt"),
        };
        (t, p)
    }

    #[test]
    fn generations_newest_first_with_the_default_and_the_running_one() {
        let (t, p) = fixture();
        let list = list(&p, &t.path().join("store/gen41")).unwrap();
        assert_eq!(list.iter().map(|g| g.number).collect::<Vec<_>>(), vec![42, 41, 40]);
        assert!(list[0].default && !list[0].running);
        assert!(list[1].running && list[1].settings);
        assert!(list[2].settings, "the manager's copy counts");
        assert!(!list[0].settings);
        assert_eq!(list[1].version, "26.05.41");
        assert_eq!(previous(&list), Some(41));
    }

    #[test]
    fn going_back_puts_the_settings_and_lock_back_and_keeps_the_old_ones() {
        let (_t, p) = fixture();
        std::fs::create_dir_all(&p.overrides).unwrap();
        std::fs::create_dir_all(&p.flake).unwrap();
        std::fs::write(p.settings_file(), "{\"version\":1,\"hostname\":\"now\"}").unwrap();
        std::fs::write(p.flake.join("flake.lock"), "lock42").unwrap();
        let done = restore_inputs(&p, 41).unwrap();
        assert_eq!(done.len(), 2);
        assert!(std::fs::read_to_string(p.settings_file()).unwrap().contains("forty-one"));
        assert_eq!(std::fs::read_to_string(p.flake.join("flake.lock")).unwrap(), "lock41");
        assert!(std::fs::read_to_string(p.overrides.join("system.json.before-recovery")).unwrap().contains("now"));
        assert_eq!(std::fs::read_to_string(p.flake.join("flake.lock.before-recovery")).unwrap(), "lock42");
        // The manager's copy when the generation carries none; no lock then.
        assert_eq!(restore_inputs(&p, 40).unwrap().len(), 1);
        assert!(std::fs::read_to_string(p.settings_file()).unwrap().contains("forty"));
        assert!(restore_inputs(&p, 42).unwrap().is_empty());
    }

    #[test]
    fn settings_shown_pretty_or_as_damaged() {
        let (_t, p) = fixture();
        assert!(settings_now(&p).starts_with("No system settings"));
        std::fs::create_dir_all(&p.overrides).unwrap();
        std::fs::write(p.settings_file(), "{\"version\":1}").unwrap();
        assert!(settings_now(&p).contains("\"version\": 1"));
        std::fs::write(p.settings_file(), "{oops").unwrap();
        assert!(settings_now(&p).starts_with("This file is damaged"));
    }
}

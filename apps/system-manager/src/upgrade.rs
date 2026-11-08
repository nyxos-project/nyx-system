//! Updates: the channel (`nix flake update` on the system's flake) and/or single tools
//! (their pins copied from the head of the channel into the overrides layer's
//! `tools.json`), built and switched to in one rebuild.
//!
//! `flake.lock` and `tools.json` are saved first and put back if anything fails; a
//! marker in the state folder says an update is running, so one cut short (a power cut)
//! is undone when the manager next starts. Every update goes into the history.

use crate::rebuild::{self, run};
use crate::{Config, Manager, generations, snapshots};
use anyhow::{Context, Result, bail, ensure};
use nyx_system_proto::{HistoryEntry, TOOLS_FILE, ToolPin, UpgradeRequest, UpgradeResult, VersionChange};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zbus::object_server::SignalEmitter;

/// Below this much free space in /nix an update doesn't start.
const MIN_FREE: u64 = 3 << 30;

const MARKER: &str = "update-in-progress.json";

/// What an update cut short needs to put back.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    before: Option<u32>,
    lock: Option<PathBuf>,
    tools: Option<PathBuf>,
}

pub fn history_file(config: &Config) -> PathBuf {
    config.state.join("history.jsonl")
}

fn tools_file(config: &Config) -> PathBuf {
    config.overrides.join("tools.json")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

async fn stage(emitter: &SignalEmitter<'_>, what: &str) {
    let _ = Manager::progress(emitter, &format!("== {what}")).await;
}

/// Put back what an update that was cut short changed, unless it got as far as switching.
pub fn recover_interrupted(config: &Config) {
    let path = config.state.join(MARKER);
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    if let Ok(m) = serde_json::from_str::<Marker>(&text) {
        if generations::current() == m.before {
            restore(config, m.lock.as_deref(), m.tools.as_deref());
            eprintln!("an update was cut short; flake.lock and tools.json were put back");
        } else {
            eprintln!("an update was cut short after switching; the new generation stays");
        }
    }
    let _ = std::fs::remove_file(path);
}

fn restore(config: &Config, lock: Option<&Path>, tools: Option<&Path>) {
    if let Some(saved) = lock {
        let _ = std::fs::copy(saved, config.flake.join("flake.lock"));
    }
    match tools {
        Some(saved) => {
            let _ = std::fs::copy(saved, tools_file(config));
        }
        None => {
            let _ = std::fs::remove_file(tools_file(config));
        }
    }
}

/// Free space in the filesystem holding `path`.
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `s` a valid statvfs to fill.
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// The pins `tools` have at the head of the channel, checked against the recipes this
/// system was built with: a pin for another recipe needs the channel update instead.
fn head_pins(config: &Config, tools: &[String]) -> Result<serde_json::Map<String, serde_json::Value>> {
    let channel = nyx_update::probe::locked_channel(&config.flake.join("flake.lock"))?;
    let url = channel.url.context("this system's flake has no channel to take tools from")?;
    let (_, source) = nyx_update::probe::fetch_head(&url)?;
    let text = std::fs::read_to_string(source.join(nyx_update::probe::SOURCES)).context("the channel has no tool pins")?;
    let head: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)?;
    let built: BTreeMap<String, ToolPin> = nyx_update::probe::read_pins(Path::new(TOOLS_FILE)).unwrap_or_default();
    let mut out = serde_json::Map::new();
    for t in tools {
        let pin = head.get(t).with_context(|| format!("the channel has no tool called {t}"))?;
        let p: ToolPin =
            serde_json::from_value(pin.clone()).with_context(|| format!("{t}'s pin in the channel can't be read"))?;
        let have = built.get(t).with_context(|| format!("this system doesn't build {t}"))?;
        ensure!(p.recipe == have.recipe, "{} {} is built differently now; it comes with the system update", p.name, p.version);
        if have.older_than(&p) {
            out.insert(t.clone(), pin.clone());
        }
    }
    Ok(out)
}

/// The programs and kernel the switch replaced: what to restart.
struct Before {
    system: Option<PathBuf>,
    closure: BTreeSet<String>,
}

fn closure(path: &Path) -> BTreeSet<String> {
    std::process::Command::new("nix-store")
        .args(["--query", "--requisites"])
        .arg(path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn current_system() -> Option<PathBuf> {
    std::fs::canonicalize("/run/current-system").ok()
}

/// The kernel, initrd or kernel modules differ from what the computer started with.
fn needs_reboot() -> bool {
    ["kernel", "initrd", "kernel-modules"].iter().any(|f| {
        let booted = std::fs::canonicalize(format!("/run/booted-system/{f}")).ok();
        let current = std::fs::canonicalize(format!("/run/current-system/{f}")).ok();
        booted.is_some() && booted != current
    })
}

/// Programs running from store paths the old system had and the new one doesn't.
fn replaced_programs(before: &Before) -> Vec<String> {
    let Some(now) = current_system() else { return Vec::new() };
    if before.system.as_ref() == Some(&now) || before.closure.is_empty() {
        return Vec::new();
    }
    let after = closure(&now);
    let gone: BTreeSet<&String> = before.closure.difference(&after).collect();
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name();
        if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else { continue };
        let exe = exe.to_string_lossy();
        let Some(rest) = exe.strip_prefix("/nix/store/") else { continue };
        let store_path = format!("/nix/store/{}", rest.split('/').next().unwrap_or_default());
        if gone.contains(&store_path)
            && let Ok(comm) = std::fs::read_to_string(entry.path().join("comm"))
        {
            names.insert(comm.trim().to_string());
        }
    }
    names.into_iter().filter(|n| !n.is_empty()).collect()
}

/// `nix store diff-closures OLD NEW`: `firefox: 132.0 → 133.0, +1.2 MiB` lines.
fn diff(before: &Before) -> Vec<VersionChange> {
    let (Some(old), Some(new)) = (&before.system, current_system()) else { return Vec::new() };
    let out = std::process::Command::new("nix").args(["store", "diff-closures"]).arg(old).arg(&new).output();
    out.map(|o| parse_diff(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

fn parse_diff(text: &str) -> Vec<VersionChange> {
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(": ")?;
            let versions = rest.split(", ").next()?;
            let (from, to) = versions.split_once(" → ")?;
            (from != to && !from.is_empty() && !to.is_empty()).then(|| VersionChange {
                name: name.trim().to_string(),
                from: from.trim().to_string(),
                to: to.trim().to_string(),
            })
        })
        .take(500)
        .collect()
}

pub async fn upgrade(config: &Config, req: &UpgradeRequest, emitter: &SignalEmitter<'_>) -> Result<UpgradeResult> {
    req.validate()?;
    let mut entry = HistoryEntry {
        time: now(),
        scope: "system".into(),
        request: req.request.clone(),
        targets: if req.channel { vec!["system".into()] } else { req.tools.clone() },
        ..HistoryEntry::default()
    };
    let result = run_upgrade(config, req, emitter).await;
    match &result {
        Ok(r) => {
            entry.result = if r.generation_after == r.generation_before { "current" } else { "updated" }.into();
            entry.changes = r.changes.clone();
            entry.generation_before = r.generation_before;
            entry.generation_after = r.generation_after;
            entry.reboot = r.reboot;
            entry.restart = r.restart.clone();
        }
        Err(e) => {
            entry.result = "failed".into();
            entry.generation_before = generations::current();
            entry.error = Some(format!("{e:#}"));
        }
    }
    if let Err(e) = nyx_system_proto::append_history(&history_file(config), &entry) {
        eprintln!("the update history couldn't be written: {e:#}");
    }
    result
}

async fn run_upgrade(config: &Config, req: &UpgradeRequest, emitter: &SignalEmitter<'_>) -> Result<UpgradeResult> {
    std::fs::create_dir_all(&config.state)?;
    if let Some(free) = free_space(Path::new("/nix")) {
        ensure!(
            free >= MIN_FREE,
            "only {:.1} GB is free for the system's files; an update needs at least 3 GB (nyx-system clean frees some)",
            free as f64 / 1e9
        );
    }
    let pins = if req.channel {
        serde_json::Map::new()
    } else {
        stage(emitter, "Fetching the tools' newest pins from the channel").await;
        let pins = head_pins(config, &req.tools)?;
        if pins.is_empty() {
            return Ok(UpgradeResult {
                summary: "Already up to date".into(),
                generation_before: generations::current(),
                generation_after: generations::current(),
                ..UpgradeResult::default()
            });
        }
        pins
    };

    // What to put back if it fails.
    let backup = config.state.join("update-backup");
    std::fs::create_dir_all(&backup)?;
    let lock = config.flake.join("flake.lock");
    let saved_lock = std::fs::copy(&lock, backup.join("flake.lock")).ok().map(|_| backup.join("flake.lock"));
    let saved_tools = std::fs::copy(tools_file(config), backup.join("tools.json")).ok().map(|_| backup.join("tools.json"));
    let before_gen = generations::current();
    let marker = Marker { before: before_gen, lock: saved_lock.clone(), tools: saved_tools.clone() };
    std::fs::write(config.state.join(MARKER), serde_json::to_string(&marker)?)?;
    rebuild::remember(config)?;
    let system = current_system();
    let before = Before { closure: system.as_deref().map(closure).unwrap_or_default(), system };

    let what = if req.channel { "an update" } else { "a tool update" };
    let pre = snapshots::before(what, emitter).await;
    let result = async {
        if req.channel {
            stage(emitter, "Downloading the newest channel").await;
            let flake = config.flake.display().to_string();
            run(emitter, &config.state, "nix", &["flake", "update", "--flake", &flake]).await?;
            // The channel brings every tool's pin.
            let _ = std::fs::remove_file(tools_file(config));
        } else {
            let mut local: serde_json::Map<String, serde_json::Value> =
                std::fs::read_to_string(tools_file(config)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            local.extend(pins.clone());
            std::fs::create_dir_all(&config.overrides)?;
            let tmp = tools_file(config).with_extension("json.new");
            std::fs::write(&tmp, serde_json::to_string_pretty(&local)? + "\n")?;
            std::fs::rename(tmp, tools_file(config))?;
        }
        rebuild::build_and_switch(config, emitter).await
    }
    .await;
    snapshots::after(pre, what, emitter).await;

    if let Err(e) = result {
        restore(config, saved_lock.as_deref(), saved_tools.as_deref());
        if generations::current() != before_gen {
            let _ = rebuild::previous_generation(emitter, &config.state).await;
        }
        let _ = std::fs::remove_file(config.state.join(MARKER));
        bail!("{e:#}; the update was undone");
    }
    let _ = std::fs::remove_file(config.state.join(MARKER));
    rebuild::remember(config)?;
    stage(emitter, "Done").await;
    let after_gen = generations::current();
    let changes = diff(&before);
    let summary = if after_gen == before_gen {
        "Already up to date".to_string()
    } else if req.channel {
        format!("Updated NyxOS ({} package{} changed)", changes.len(), if changes.len() == 1 { "" } else { "s" })
    } else {
        let names: Vec<String> = pins
            .iter()
            .map(|(k, v)| format!("{} {}", v["name"].as_str().unwrap_or(k), v["version"].as_str().unwrap_or("")))
            .collect();
        format!("Updated {}", names.join(", "))
    };
    Ok(UpgradeResult {
        summary,
        generation_before: before_gen,
        generation_after: after_gen,
        reboot: needs_reboot(),
        restart: replaced_programs(&before),
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_diffs_become_version_changes() {
        let text = "firefox: 132.0 → 133.0, +1.2 MiB\nlinux: 6.12.1 → 6.12.4\nnew-thing: ∅ → 1.0, +3 KiB\nsame: 1.0 → 1.0\nnot a diff line\n";
        let c = parse_diff(text);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], VersionChange { name: "firefox".into(), from: "132.0".into(), to: "133.0".into() });
        assert_eq!(c[2].from, "∅");
    }

    #[test]
    fn an_interrupted_update_is_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            flake: dir.path().join("flake"),
            host: "h".into(),
            overrides: dir.path().join("overrides"),
            state: dir.path().join("state"),
        };
        for d in [&config.flake, &config.overrides, &config.state] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(config.flake.join("flake.lock"), "new").unwrap();
        std::fs::write(tools_file(&config), "{\"new\": {}}").unwrap();
        let saved = config.state.join("lock.saved");
        std::fs::write(&saved, "old").unwrap();
        // No generations here, so `before` (None) is still current: nothing switched.
        let m = Marker { before: generations::current(), lock: Some(saved), tools: None };
        std::fs::write(config.state.join(MARKER), serde_json::to_string(&m).unwrap()).unwrap();
        recover_interrupted(&config);
        assert_eq!(std::fs::read_to_string(config.flake.join("flake.lock")).unwrap(), "old");
        assert!(!tools_file(&config).exists(), "there was no tools.json before");
        assert!(!config.state.join(MARKER).exists());
    }
}

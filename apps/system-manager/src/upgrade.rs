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
use nyx_system_proto::{HistoryEntry, TOOLS_FILE, ToolPin, UpgradeRequest, UpgradeResult, UpgradeStage, VersionChange};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
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
    running_from(&before.closure, &closure(&now))
}

/// Programs running from store paths in `old` that `new` doesn't have.
fn running_from(old: &BTreeSet<String>, new: &BTreeSet<String>) -> Vec<String> {
    if old.is_empty() {
        return Vec::new();
    }
    let gone: BTreeSet<&String> = old.difference(new).collect();
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
    diff_paths(old, &new)
}

fn diff_paths(old: &Path, new: &Path) -> Vec<VersionChange> {
    let out = std::process::Command::new("nix").args(["store", "diff-closures"]).arg(old).arg(new).output();
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
    // Preparing and setting aside change nothing: no history entry.
    if matches!(req.stage, UpgradeStage::Prepare | UpgradeStage::Discard) {
        return run_upgrade(config, req, &mut BTreeMap::new(), emitter).await;
    }
    let mut entry = HistoryEntry {
        time: now(),
        scope: "system".into(),
        request: req.request.clone(),
        targets: if req.channel { vec!["system".into()] } else { req.tools.clone() },
        ..HistoryEntry::default()
    };
    let start = Instant::now();
    let mut result = run_upgrade(config, req, &mut entry.seconds, emitter).await;
    entry.seconds.insert("total".into(), start.elapsed().as_secs());
    if let Ok(r) = &mut result {
        r.seconds.clone_from(&entry.seconds);
    }
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

async fn run_upgrade(
    config: &Config,
    req: &UpgradeRequest,
    seconds: &mut BTreeMap<String, u64>,
    emitter: &SignalEmitter<'_>,
) -> Result<UpgradeResult> {
    match req.stage {
        UpgradeStage::Discard => return discard(config),
        UpgradeStage::Finish => return finish(config, seconds, emitter).await,
        UpgradeStage::Full | UpgradeStage::Prepare => {}
    }
    let mut timed = |stage: &str, since: Instant| {
        seconds.insert(stage.to_string(), since.elapsed().as_secs());
    };
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
        let t = Instant::now();
        let pins = head_pins(config, &req.tools);
        timed("pins", t);
        let pins = pins?;
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
    // A new prepare replaces one waiting.
    let _ = std::fs::remove_dir_all(prepared_dir(config));

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
    let prepare = req.stage == UpgradeStage::Prepare;
    // Snapshots go with switching, which preparing doesn't do.
    let pre = if prepare { Vec::new() } else { snapshots::before(what, emitter).await };
    let result = async {
        let t = Instant::now();
        let r = fetch(config, req, &pins, emitter).await;
        timed("download", t);
        r?;
        // Building also downloads what the new system needs.
        let t = Instant::now();
        let r = rebuild::build(config, emitter).await;
        timed("build", t);
        r?;
        if prepare {
            return Ok(());
        }
        let t = Instant::now();
        let r = rebuild::switch(config, emitter).await;
        timed("switch", t);
        r
    }
    .await;
    if !prepare {
        snapshots::after(pre, what, emitter).await;
    }

    if let Err(e) = result {
        restore(config, saved_lock.as_deref(), saved_tools.as_deref());
        if generations::current() != before_gen {
            let _ = rebuild::previous_generation(emitter, &config.state).await;
        }
        let _ = std::fs::remove_file(config.state.join(MARKER));
        bail!("{e:#}; the update was undone");
    }
    let names: Vec<String> = pins
        .iter()
        .map(|(k, v)| format!("{} {}", v["name"].as_str().unwrap_or(k), v["version"].as_str().unwrap_or("")))
        .collect();
    if prepare {
        let prepared = keep_prepared(config, req, &before, before_gen, names);
        // The system stays as it was until the person finishes: the new lock and pins
        // wait aside, so another change in the meantime doesn't take the update along.
        restore(config, saved_lock.as_deref(), saved_tools.as_deref());
        let _ = std::fs::remove_file(config.state.join(MARKER));
        rebuild::remember(config)?;
        stage(emitter, "Ready").await;
        return prepared;
    }
    let _ = std::fs::remove_file(config.state.join(MARKER));
    rebuild::remember(config)?;
    stage(emitter, "Done").await;
    Ok(switched(req.channel, &names, &before, before_gen))
}

/// The channel update (and its signature, on stable), or the tools' new pins, into the
/// system's flake. Undone by the caller if anything after fails.
async fn fetch(
    config: &Config,
    req: &UpgradeRequest,
    pins: &serde_json::Map<String, serde_json::Value>,
    emitter: &SignalEmitter<'_>,
) -> Result<()> {
    if req.channel {
        stage(emitter, "Downloading the newest channel").await;
        let flake = config.flake.display().to_string();
        run(emitter, &config.state, "nix", &["flake", "update", "--flake", &flake]).await?;
        // Stable moves only to a signed release; otherwise this is undone.
        let lock = config.flake.join("flake.lock");
        let locked = std::fs::read_to_string(&lock).ok().and_then(|t| nyx_update::signed::channel_in_lock(&t));
        if let Some(channel) = locked.filter(|c| c.needs_signature(Path::new(nyx_update::signed::SIGNERS))) {
            stage(emitter, "Checking the release signature").await;
            let tag = tokio::task::spawn_blocking(move || {
                nyx_update::signed::verify(&channel.url(), &channel.rev, Path::new(nyx_update::signed::SIGNERS))
            })
            .await??;
            eprintln!("the channel is the signed release {tag}");
        }
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
    Ok(())
}

/// What a finished update reports.
fn switched(channel: bool, names: &[String], before: &Before, before_gen: Option<u32>) -> UpgradeResult {
    let after_gen = generations::current();
    let changes = diff(before);
    let summary = if after_gen == before_gen {
        "Already up to date".to_string()
    } else if channel {
        format!("Updated NyxOS ({} package{} changed)", changes.len(), if changes.len() == 1 { "" } else { "s" })
    } else {
        format!("Updated {}", names.join(", "))
    };
    UpgradeResult {
        summary,
        generation_before: before_gen,
        generation_after: after_gen,
        reboot: needs_reboot(),
        restart: replaced_programs(before),
        changes,
        ..UpgradeResult::default()
    }
}

/// An update built and waiting for the person (`update-prepared/`): the new flake.lock
/// and tools.json, and what was built from them.
fn prepared_dir(config: &Config) -> PathBuf {
    config.state.join("update-prepared")
}

/// A prepared update waits this long; after it, finishing asks for a new check.
const PREPARED_FOR: i64 = 24 * 3600;

#[derive(Debug, Serialize, Deserialize)]
struct Prepared {
    time: i64,
    channel: bool,
    request: String,
    built: PathBuf,
    /// tools.json as the update had it (absent when the channel removed it).
    tools: bool,
    names: Vec<String>,
    before: Option<u32>,
}

/// Keep the new lock, pins and build aside, and say what switching to them would change.
fn keep_prepared(
    config: &Config,
    req: &UpgradeRequest,
    before: &Before,
    before_gen: Option<u32>,
    names: Vec<String>,
) -> Result<UpgradeResult> {
    let built = std::fs::canonicalize(config.state.join("result")).context("the build left no result")?;
    let dir = prepared_dir(config);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(config.flake.join("flake.lock"), dir.join("flake.lock"))?;
    let tools = std::fs::copy(tools_file(config), dir.join("tools.json")).is_ok();
    let meta = Prepared {
        time: now(),
        channel: req.channel,
        request: req.request.clone(),
        built: built.clone(),
        tools,
        names,
        before: before_gen,
    };
    std::fs::write(dir.join("prepared.json"), serde_json::to_string_pretty(&meta)?)?;
    if before.system.as_ref() == Some(&built) {
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(UpgradeResult {
            summary: "Already up to date".into(),
            generation_before: before_gen,
            generation_after: before_gen,
            ..UpgradeResult::default()
        });
    }
    let changes = before.system.as_deref().map(|old| diff_paths(old, &built)).unwrap_or_default();
    let size = |p: &Path| closure_size(p);
    let size_change = match (before.system.as_deref().and_then(size), size(&built)) {
        (Some(a), Some(b)) => Some(b as i64 - a as i64),
        _ => None,
    };
    let after = closure(&built);
    Ok(UpgradeResult {
        summary: format!("Ready to update: {} package{} change", changes.len(), if changes.len() == 1 { "" } else { "s" }),
        generation_before: before_gen,
        generation_after: before_gen,
        reboot: boots_differently(&built),
        restart: running_from(&before.closure, &after),
        changes,
        prepared: true,
        size_change,
        ..UpgradeResult::default()
    })
}

/// Switch to the prepared update: its lock and pins back in place, built again (the
/// store has it: nothing is downloaded) and switched to only if that's what was shown.
async fn finish(config: &Config, seconds: &mut BTreeMap<String, u64>, emitter: &SignalEmitter<'_>) -> Result<UpgradeResult> {
    let dir = prepared_dir(config);
    let meta: Prepared = std::fs::read_to_string(dir.join("prepared.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .context("no update is waiting; check for updates again")?;
    if now() - meta.time > PREPARED_FOR || generations::current() != meta.before {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("the prepared update is out of date; check for updates again");
    }
    let backup = config.state.join("update-backup");
    std::fs::create_dir_all(&backup)?;
    let lock = config.flake.join("flake.lock");
    let saved_lock = std::fs::copy(&lock, backup.join("flake.lock")).ok().map(|_| backup.join("flake.lock"));
    let saved_tools = std::fs::copy(tools_file(config), backup.join("tools.json")).ok().map(|_| backup.join("tools.json"));
    let before_gen = generations::current();
    std::fs::write(
        config.state.join(MARKER),
        serde_json::to_string(&Marker { before: before_gen, lock: saved_lock.clone(), tools: saved_tools.clone() })?,
    )?;
    let system = current_system();
    let before = Before { closure: system.as_deref().map(closure).unwrap_or_default(), system };
    let what = if meta.channel { "an update" } else { "a tool update" };
    let pre = snapshots::before(what, emitter).await;
    let result = async {
        std::fs::copy(dir.join("flake.lock"), &lock)?;
        if meta.tools {
            std::fs::create_dir_all(&config.overrides)?;
            std::fs::copy(dir.join("tools.json"), tools_file(config))?;
        } else {
            let _ = std::fs::remove_file(tools_file(config));
        }
        let t = Instant::now();
        rebuild::build(config, emitter).await?;
        seconds.insert("build".into(), t.elapsed().as_secs());
        let built = std::fs::canonicalize(config.state.join("result")).context("the build left no result")?;
        ensure!(built == meta.built, "the system changed since this update was prepared; check for updates again");
        let t = Instant::now();
        rebuild::switch(config, emitter).await?;
        seconds.insert("switch".into(), t.elapsed().as_secs());
        anyhow::Ok(())
    }
    .await;
    snapshots::after(pre, what, emitter).await;
    let _ = std::fs::remove_dir_all(&dir);
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
    Ok(switched(meta.channel, &meta.names, &before, before_gen))
}

/// Forget a prepared update; the system never had it.
fn discard(config: &Config) -> Result<UpgradeResult> {
    let had = prepared_dir(config).exists();
    let _ = std::fs::remove_dir_all(prepared_dir(config));
    Ok(UpgradeResult {
        summary: if had { "The update was set aside; nothing changed" } else { "No update was waiting" }.into(),
        generation_before: generations::current(),
        generation_after: generations::current(),
        ..UpgradeResult::default()
    })
}

/// `nix path-info --closure-size`: how much space a system takes with everything it needs.
fn closure_size(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("nix").args(["path-info", "--closure-size"]).arg(path).output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1)?.parse().ok()
}

/// The kernel, initrd or kernel modules of `system` differ from what the computer started with.
fn boots_differently(system: &Path) -> bool {
    ["kernel", "initrd", "kernel-modules"].iter().any(|f| {
        let booted = std::fs::canonicalize(format!("/run/booted-system/{f}")).ok();
        let new = std::fs::canonicalize(system.join(f)).ok();
        booted.is_some() && booted != new
    })
}

pub async fn rollback(config: &Config, emitter: &SignalEmitter<'_>) -> Result<String> {
    let before = generations::current();
    let start = Instant::now();
    let result = rebuild::rollback(config, emitter).await;
    let entry = HistoryEntry {
        time: now(),
        scope: "system".into(),
        request: "rollback".into(),
        targets: vec!["system".into()],
        generation_before: before,
        generation_after: generations::current(),
        result: if result.is_ok() { "rolled-back" } else { "failed" }.into(),
        reboot: result.is_ok() && needs_reboot(),
        error: result.as_ref().err().map(|e| format!("{e:#}")),
        seconds: BTreeMap::from([("total".to_string(), start.elapsed().as_secs())]),
        ..HistoryEntry::default()
    };
    if let Err(e) = nyx_system_proto::append_history(&history_file(config), &entry) {
        eprintln!("the update history couldn't be written: {e:#}");
    }
    result
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

//! Building and switching the system, and undoing a change that doesn't take.

use crate::{Config, generations, snapshots};
use anyhow::{Context, Result, bail};
use nyx_system_proto::Change;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use zbus::object_server::SignalEmitter;

const PROFILE: &str = "/nix/var/nix/profiles/system";

/// What a command is, as progress shows it: building and switching the system in NyxOS's
/// words, anything else as the command.
fn shown(program: &str, args: &[&str]) -> String {
    if !program.ends_with("nixos-rebuild") {
        return format!("$ {program} {}", args.join(" "));
    }
    match args.first() {
        Some(&"switch") => "Building the new system and switching to it…",
        Some(&"boot") => "Building the new system for the next start…",
        _ => "Building the new system…",
    }
    .into()
}

/// Run a command as root, sending each line it prints as progress.
pub async fn run(emitter: &SignalEmitter<'_>, cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    let _ = crate::Manager::progress(emitter, &shown(program, args)).await;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't start {program}"))?;
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    // The first "error:" line says why; otherwise the last line does.
    let mut last = String::new();
    let mut error = String::new();
    let (mut out_done, mut err_done) = (false, false);
    while !(out_done && err_done) {
        tokio::select! {
            line = out.next_line(), if !out_done => match line? {
                Some(l) => { let _ = crate::Manager::progress(emitter, &l).await; }
                None => out_done = true,
            },
            line = err.next_line(), if !err_done => match line? {
                Some(l) => {
                    let t = l.trim();
                    if error.is_empty() && t.starts_with("error:") {
                        error = t.trim_start_matches("error:").trim().to_string();
                    }
                    if !t.is_empty() { last = t.to_string(); }
                    let _ = crate::Manager::progress(emitter, &l).await;
                }
                None => err_done = true,
            },
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        let why = [error, last].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| status.to_string());
        if program.ends_with("nixos-rebuild") {
            bail!("The new system couldn't be built: {why}");
        }
        bail!("{program} failed: {why}");
    }
    Ok(())
}

/// `nixos-rebuild VERB` for this machine, with the overrides layer as the flake's input.
fn rebuild_args<'a>(verb: &'a str, flake: &'a str, overrides: &'a str) -> [&'a str; 6] {
    [verb, "--flake", flake, "--override-input", "nyxos-overrides", overrides]
}

/// Build the configuration from the overrides layer `overrides`, then switch to it.
/// Progress lines starting `== ` name the stage.
async fn build_and_switch(config: &Config, overrides: &Path, emitter: &SignalEmitter<'_>) -> Result<()> {
    rebuild(config, overrides, "build", "Building the new system", emitter).await?;
    rebuild(config, overrides, "switch", "Switching to it", emitter).await
}

/// `nixos-rebuild VERB` for this machine, its stage named first.
async fn rebuild(config: &Config, overrides: &Path, verb: &str, stage: &str, emitter: &SignalEmitter<'_>) -> Result<()> {
    std::fs::create_dir_all(&config.state)?;
    let flake = format!("path:{}#{}", config.flake.display(), config.host);
    let overrides = format!("path:{}", overrides.display());
    let _ = crate::Manager::progress(emitter, &format!("== {stage}")).await;
    run(emitter, &config.state, "nixos-rebuild", &rebuild_args(verb, &flake, &overrides)).await
}

pub async fn build(config: &Config, emitter: &SignalEmitter<'_>) -> Result<()> {
    rebuild(config, &config.overrides, "build", "Building the new system", emitter).await
}

pub async fn switch(config: &Config, emitter: &SignalEmitter<'_>) -> Result<()> {
    rebuild(config, &config.overrides, "switch", "Switching to it", emitter).await
}

/// A change is built from a copy of the overrides layer with its settings
/// (`staged-overrides`); the layer itself gets them only once the switch worked, so a
/// change cut short (a crash, a power cut) never waits there for the next one.
fn staged_dir(config: &Config) -> std::path::PathBuf {
    config.state.join("staged-overrides")
}

/// The generation the staged change started from, kept beside it.
const STAGED_FROM: &str = ".nyx-staged-from";

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    let Ok(entries) = std::fs::read_dir(from) else { return Ok(()) };
    for entry in entries {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn stage(config: &Config, new: &nyx_system_proto::SystemSettings, before: Option<u32>) -> Result<std::path::PathBuf> {
    let dir = staged_dir(config);
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&config.overrides, &dir)?;
    let staged = Config { overrides: dir.clone(), ..config.clone() };
    staged.write_settings(new)?;
    std::fs::write(dir.join(STAGED_FROM), before.map(|g| g.to_string()).unwrap_or_default())?;
    Ok(dir)
}

/// The staged settings into the overrides layer.
fn commit_staged(config: &Config, dir: &Path) -> Result<()> {
    let text = std::fs::read_to_string(dir.join("system.json"))?;
    config.write_settings(&nyx_system_proto::SystemSettings::parse(&text)?)?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

/// A change the manager didn't see through: kept if the system switched to it, else
/// dropped. Run at start.
pub fn recover_staged(config: &Config) {
    let dir = staged_dir(config);
    let Ok(from) = std::fs::read_to_string(dir.join(STAGED_FROM)) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let before = from.trim().parse::<u32>().ok();
    if generations::current() != before && commit_staged(config, &dir).is_ok() {
        eprintln!("a system change was cut short after switching; its settings were kept");
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("a system change was cut short before switching; it was dropped");
    }
}

/// Keep the settings the current generation was built from, for rollback.
pub fn remember(config: &Config) -> Result<()> {
    if let Some(current) = generations::list()?.into_iter().find(|g| g.current) {
        let dir = config.state.join("generations");
        std::fs::create_dir_all(&dir)?;
        let text = std::fs::read_to_string(config.settings_file()).unwrap_or_else(|_| "{\"version\":1}\n".into());
        std::fs::write(dir.join(format!("{}.json", current.number)), text)?;
    }
    Ok(())
}

/// Back to the generation before the current one.
pub async fn previous_generation(emitter: &SignalEmitter<'_>, cwd: &Path) -> Result<()> {
    run(emitter, cwd, "nix-env", &["--profile", PROFILE, "--rollback"]).await?;
    run(emitter, cwd, &format!("{PROFILE}/bin/switch-to-configuration"), &["switch"]).await
}

pub async fn apply(config: &Config, changes: &[Change], emitter: &SignalEmitter<'_>) -> Result<String> {
    let old = config.read_settings()?;
    let new = old.with(changes)?;
    if new == old {
        return Ok("Nothing to change".into());
    }
    // The generation the system is on now keeps its settings, in case this one is rolled back.
    remember(config)?;
    let before = generations::current();
    let staged = stage(config, &new, before)?;
    let pre = snapshots::before("a change to the system settings", emitter).await;
    let result = build_and_switch(config, &staged, emitter).await;
    snapshots::after(pre, "a change to the system settings", emitter).await;
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staged);
        // A switch that failed may have moved the profile already.
        if generations::current() != before {
            let _ = previous_generation(emitter, &config.state).await;
        }
        return Err(anyhow::anyhow!("{e:#}; the change was undone, so nothing on the system changed"));
    }
    commit_staged(config, &staged)?;
    remember(config)?;
    Ok("Applied".into())
}

pub async fn rollback(config: &Config, emitter: &SignalEmitter<'_>) -> Result<String> {
    let list = generations::list()?;
    let current = list.iter().find(|g| g.current).map(|g| g.number).context("no current generation")?;
    let previous = list.iter().filter(|g| g.number < current).map(|g| g.number).max().context("there's no earlier generation")?;
    let pre = snapshots::before("going back a generation", emitter).await;
    let result = previous_generation(emitter, &config.state).await;
    snapshots::after(pre, "going back a generation", emitter).await;
    result?;
    // The settings that generation was built from, so the next change starts from them.
    let saved = config.state.join("generations").join(format!("{previous}.json"));
    if let Ok(text) = std::fs::read_to_string(saved) {
        config.write_settings(&nyx_system_proto::SystemSettings::parse(&text)?)?;
    }
    // And its versions: otherwise the next change would bring the newer channel or tools
    // back.
    restore_versions(config, &Path::new(&format!("{PROFILE}-{previous}-link")).join("etc/nyxos/built-from"))?;
    Ok(format!("Back on generation {previous}"))
}

/// Put back the flake.lock and tool pins a generation was built from (`built-from`),
/// when it carries them.
fn restore_versions(config: &Config, built_from: &Path) -> Result<()> {
    let Ok(lock) = std::fs::read(built_from.join("flake.lock")) else { return Ok(()) };
    std::fs::write(config.flake.join("flake.lock"), lock)?;
    let tools = config.overrides.join("tools.json");
    match std::fs::read(built_from.join("tools.json")) {
        Ok(pins) => std::fs::write(&tools, pins)?,
        Err(_) if tools.exists() => std::fs::remove_file(&tools)?,
        Err(_) => {}
    }
    Ok(())
}

/// Remove system generations older than `days` (never the current one) and whatever
/// no generation needs any more, then refresh the boot menu. Returns what nix freed.
pub async fn collect_garbage(config: &Config, days: u32, emitter: &SignalEmitter<'_>) -> Result<String> {
    anyhow::ensure!((1..=3650).contains(&days), "keep between a day and ten years");
    let cwd = &config.state;
    run(emitter, cwd, "nix-env", &["--profile", PROFILE, "--delete-generations", &format!("{days}d")]).await?;
    let out = tokio::process::Command::new("nix-store").args(["--gc"]).current_dir(cwd).output().await?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    for l in text.lines() {
        let _ = crate::Manager::progress(emitter, l).await;
    }
    anyhow::ensure!(out.status.success(), "nix-store --gc failed");
    run(emitter, cwd, &format!("{PROFILE}/bin/switch-to-configuration"), &["boot"]).await?;
    generations::forget_missing(&config.state);
    Ok(text.lines().rev().find(|l| l.contains("freed")).unwrap_or("Cleaned up").trim().to_string())
}

/// Shrink the system journal to `megabytes`.
pub async fn vacuum_journal(config: &Config, megabytes: u32, emitter: &SignalEmitter<'_>) -> Result<String> {
    anyhow::ensure!((50..=100_000).contains(&megabytes), "keep between 50 MB and 100 GB of logs");
    run(emitter, &config.state, "journalctl", &[&format!("--vacuum-size={megabytes}M")]).await?;
    Ok(format!("The system log is at most {megabytes} MB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_change_reaches_the_overrides_only_when_committed() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            flake: dir.path().join("flake"),
            host: "h".into(),
            overrides: dir.path().join("overrides"),
            state: dir.path().join("state"),
        };
        std::fs::create_dir_all(&config.overrides).unwrap();
        std::fs::write(config.overrides.join("appearance.toml"), "theme = \"x\"\n").unwrap();
        let old = config.read_settings().unwrap();
        let new = old.with(&[Change::Service { name: "fwupd".into(), enabled: Some(true) }]).unwrap();
        let staged = stage(&config, &new, Some(7)).unwrap();
        assert_eq!(config.read_settings().unwrap(), old);
        assert!(staged.join("appearance.toml").exists());
        commit_staged(&config, &staged).unwrap();
        assert_eq!(config.read_settings().unwrap(), new);
        assert!(!staged.exists());
    }

    #[test]
    fn going_back_puts_the_versions_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            flake: dir.path().join("flake"),
            host: "h".into(),
            overrides: dir.path().join("overrides"),
            state: dir.path().join("state"),
        };
        let built_from = dir.path().join("gen/etc/nyxos/built-from");
        for d in [&config.flake, &config.overrides, &built_from] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(config.flake.join("flake.lock"), "new").unwrap();
        std::fs::write(config.overrides.join("tools.json"), "{\"claude-code\": {}}").unwrap();
        // A generation that carries nothing leaves things as they are.
        restore_versions(&config, &dir.path().join("none")).unwrap();
        assert!(config.overrides.join("tools.json").exists());
        // One built from the channel's pins alone.
        std::fs::write(built_from.join("flake.lock"), "old").unwrap();
        restore_versions(&config, &built_from).unwrap();
        assert_eq!(std::fs::read_to_string(config.flake.join("flake.lock")).unwrap(), "old");
        assert!(!config.overrides.join("tools.json").exists());
        std::fs::write(built_from.join("tools.json"), "{\"codex\": {}}").unwrap();
        restore_versions(&config, &built_from).unwrap();
        assert_eq!(std::fs::read_to_string(config.overrides.join("tools.json")).unwrap(), "{\"codex\": {}}");
    }
}

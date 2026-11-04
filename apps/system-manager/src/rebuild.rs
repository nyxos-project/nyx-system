//! Building and switching the system, and undoing a change that doesn't take.

use crate::{Config, generations};
use anyhow::{Context, Result, bail};
use nyx_system_proto::Change;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use zbus::object_server::SignalEmitter;

const PROFILE: &str = "/nix/var/nix/profiles/system";

/// Run a command as root, sending each line it prints as progress.
pub async fn run(emitter: &SignalEmitter<'_>, cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    let _ = crate::Manager::progress(emitter, &format!("$ {program} {}", args.join(" "))).await;
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
        bail!("{program} failed: {why}");
    }
    Ok(())
}

/// `nixos-rebuild VERB` for this machine, with the overrides layer as the flake's input.
fn rebuild_args<'a>(verb: &'a str, flake: &'a str, overrides: &'a str) -> [&'a str; 6] {
    [verb, "--flake", flake, "--override-input", "nyxos-overrides", overrides]
}

/// Build the configuration, then switch to it.
async fn build_and_switch(config: &Config, emitter: &SignalEmitter<'_>) -> Result<()> {
    std::fs::create_dir_all(&config.state)?;
    let flake = format!("path:{}#{}", config.flake.display(), config.host);
    let overrides = format!("path:{}", config.overrides.display());
    run(emitter, &config.state, "nixos-rebuild", &rebuild_args("build", &flake, &overrides)).await?;
    run(emitter, &config.state, "nixos-rebuild", &rebuild_args("switch", &flake, &overrides)).await
}

/// Keep the settings the current generation was built from, for rollback.
fn remember(config: &Config) -> Result<()> {
    if let Some(current) = generations::list()?.into_iter().find(|g| g.current) {
        let dir = config.state.join("generations");
        std::fs::create_dir_all(&dir)?;
        let text = std::fs::read_to_string(config.settings_file()).unwrap_or_else(|_| "{\"version\":1}\n".into());
        std::fs::write(dir.join(format!("{}.json", current.number)), text)?;
    }
    Ok(())
}

/// Back to the generation before the current one.
async fn previous_generation(emitter: &SignalEmitter<'_>, cwd: &Path) -> Result<()> {
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
    config.write_settings(&new)?;
    if let Err(e) = build_and_switch(config, emitter).await {
        config.write_settings(&old)?;
        // A switch that failed may have moved the profile already.
        if generations::current() != before {
            let _ = previous_generation(emitter, &config.state).await;
        }
        return Err(anyhow::anyhow!("{e:#}; the change was undone, so nothing on the system changed"));
    }
    remember(config)?;
    Ok("Applied".into())
}

pub async fn update(config: &Config, emitter: &SignalEmitter<'_>) -> Result<String> {
    let lock = config.flake.join("flake.lock");
    let saved = std::fs::read(&lock).ok();
    remember(config)?;
    let before = generations::current();
    let flake = config.flake.display().to_string();
    let result = async {
        run(emitter, &config.state, "nix", &["flake", "update", "--flake", &flake]).await?;
        build_and_switch(config, emitter).await
    }
    .await;
    if let Err(e) = result {
        if let Some(bytes) = saved {
            std::fs::write(&lock, bytes)?;
        }
        if generations::current() != before {
            let _ = previous_generation(emitter, &config.state).await;
        }
        return Err(anyhow::anyhow!("{e:#}; the update was undone"));
    }
    remember(config)?;
    Ok("Updated".into())
}

pub async fn rollback(config: &Config, emitter: &SignalEmitter<'_>) -> Result<String> {
    let list = generations::list()?;
    let current = list.iter().find(|g| g.current).map(|g| g.number).context("no current generation")?;
    let previous = list.iter().filter(|g| g.number < current).map(|g| g.number).max().context("there's no earlier generation")?;
    previous_generation(emitter, &config.state).await?;
    // The settings that generation was built from, so the next change starts from them.
    let saved = config.state.join("generations").join(format!("{previous}.json"));
    if let Ok(text) = std::fs::read_to_string(saved) {
        config.write_settings(&nyx_system_proto::SystemSettings::parse(&text)?)?;
    }
    Ok(format!("Back on generation {previous}"))
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

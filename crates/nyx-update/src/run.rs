//! Carrying out a [`Plan`]: the system step through the System Manager (its progress
//! streamed), the user's Nix profile and Flatpaks as the user, firmware through fwupd.
//! One update at a time per user; each step's outcome is reported, and a step that fails
//! doesn't stop the rest. The user's steps go into the user's history; the System Manager
//! keeps the system's.

use crate::{Plan, Step};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use nyx_system_proto::{HistoryEntry, SystemProxy, UpgradeRequest, UpgradeResult};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// How one step went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOutcome {
    pub step: Step,
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<UpgradeResult>,
}

/// How the whole plan went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub steps: Vec<StepOutcome>,
    /// A restart of the computer finishes it.
    pub reboot: bool,
    /// Programs to reopen.
    pub restart: Vec<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.ok)
    }
}

/// Held while an update runs: one per user at a time (the System Manager also runs one
/// system change at a time).
pub struct Lock(#[allow(dead_code)] std::fs::File);

pub fn lock() -> Result<Lock> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join("nyx-update.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("can't open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock(file)),
        Err(std::fs::TryLockError::WouldBlock) => bail!("another update is already running"),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// The user's update history.
pub fn user_history() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("nyx/update-history.jsonl")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The system's history (from the System Manager) and the user's, newest first.
pub fn merged_history(system: Vec<HistoryEntry>, limit: usize) -> Vec<HistoryEntry> {
    let mut all = system;
    all.extend(nyx_system_proto::read_history(&user_history(), limit));
    all.sort_by_key(|e| std::cmp::Reverse(e.time));
    all.truncate(limit);
    all
}

async fn proxy() -> Result<SystemProxy<'static>> {
    let conn = zbus::Connection::system().await.context("can't reach the system bus")?;
    Ok(SystemProxy::new(&conn).await?)
}

/// Run `plan`; `line` gets progress as it comes (lines starting `== ` name a stage).
/// `firmware` says whether the firmware step may run (the person agreed to it).
pub async fn execute(plan: &Plan, firmware: bool, line: &mut dyn FnMut(&str)) -> Result<Outcome> {
    let _lock = lock()?;
    let mut out = Outcome::default();
    for step in &plan.steps {
        let start = std::time::Instant::now();
        let result = match step {
            Step::System { channel, tools } => system(plan, *channel, tools, line).await,
            Step::Profile { names } => {
                let result = profile(names, line).map(|(message, running)| {
                    out.restart.extend(running);
                    message
                });
                user_step(plan, step, result, start)
            }
            Step::Flatpak { apps } => user_step(plan, step, flatpak(apps, line), start),
            Step::Mise { tools } => user_step(plan, step, mise(tools, line), start),
            Step::UvTools { tools } => user_step(plan, step, uv(tools, line), start),
            Step::Firmware { devices } if firmware => user_step(plan, step, fwupd(devices, line), start),
            Step::Firmware { .. } => Ok((String::from("Firmware wasn't updated: it's only updated when you agree to it."), None)),
        };
        let outcome = match result {
            Ok((message, system)) => StepOutcome { step: step.clone(), ok: true, message, system },
            Err(e) => StepOutcome { step: step.clone(), ok: false, message: format!("{e:#}"), system: None },
        };
        if let Some(r) = &outcome.system {
            out.reboot |= r.reboot;
            out.restart.extend(r.restart.iter().cloned());
        }
        if matches!(step, Step::Firmware { .. }) && outcome.ok && firmware {
            out.reboot |= plan.may_restart;
        }
        out.steps.push(outcome);
    }
    out.restart.sort();
    out.restart.dedup();
    Ok(out)
}

async fn system(
    plan: &Plan,
    channel: bool,
    tools: &[String],
    line: &mut dyn FnMut(&str),
) -> Result<(String, Option<UpgradeResult>)> {
    let request = UpgradeRequest { channel, tools: tools.to_vec(), request: plan.request.clone() };
    request.validate()?;
    let p = proxy().await?;
    let mut lines = p.receive_progress().await?;
    let json = serde_json::to_string(&request)?;
    let call = p.upgrade(&json);
    tokio::pin!(call);
    let reply = loop {
        tokio::select! {
            r = &mut call => break r,
            Some(signal) = lines.next() => {
                if let Ok(args) = signal.args() {
                    line(args.line());
                }
            }
        }
    };
    let reply = reply.map_err(|e| match e {
        zbus::Error::MethodError(_, Some(text), _) => anyhow::anyhow!(text),
        e => e.into(),
    })?;
    let result: UpgradeResult = serde_json::from_str(&reply).context("the System Manager's answer couldn't be read")?;
    Ok((result.summary.clone(), Some(result)))
}

/// Record a user step in the user's history, with how long it and the check took.
fn user_step(
    plan: &Plan,
    step: &Step,
    result: Result<String>,
    start: std::time::Instant,
) -> Result<(String, Option<UpgradeResult>)> {
    let (targets, scope) = match step {
        Step::Profile { names } if names.is_empty() => (vec!["nix profile".to_string()], "user"),
        Step::Profile { names } => (names.clone(), "user"),
        Step::Flatpak { apps } if apps.is_empty() => (vec!["flatpak".to_string()], "user"),
        Step::Flatpak { apps } => (apps.clone(), "user"),
        Step::Firmware { devices } => (devices.clone(), "firmware"),
        Step::Mise { tools } if tools.is_empty() => (vec!["mise".to_string()], "user"),
        Step::UvTools { tools } if tools.is_empty() => (vec!["uv tools".to_string()], "user"),
        Step::Mise { tools } | Step::UvTools { tools } => (tools.clone(), "user"),
        Step::System { .. } => (Vec::new(), "system"),
    };
    let entry = HistoryEntry {
        time: now(),
        scope: scope.into(),
        request: plan.request.clone(),
        targets,
        result: if result.is_ok() { "updated" } else { "failed" }.into(),
        reboot: matches!(step, Step::Firmware { .. }) && plan.may_restart && result.is_ok(),
        error: result.as_ref().err().map(|e| format!("{e:#}")),
        seconds: std::collections::BTreeMap::from([
            ("check".to_string(), plan.check_seconds),
            ("total".to_string(), start.elapsed().as_secs()),
        ]),
        ..HistoryEntry::default()
    };
    let _ = nyx_system_proto::append_history(&user_history(), &entry);
    result.map(|m| (m, None))
}

/// Run a command, sending each line it prints to `line`; the first `error:` line (or
/// the last line) says why it failed.
fn stream(program: &str, args: &[String], line: &mut dyn FnMut(&str)) -> Result<()> {
    stream_in(program, args, None, line)
}

fn stream_in(program: &str, args: &[String], cwd: Option<&std::path::Path>, line: &mut dyn FnMut(&str)) -> Result<()> {
    line(&format!("$ {program} {}", args.join(" ")));
    let mut cmd = Command::new(program);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't start {program}"))?;
    let err = child
        .stderr
        .take()
        .map(|e| std::thread::spawn(move || BufReader::new(e).lines().map_while(Result::ok).collect::<Vec<_>>()));
    if let Some(o) = child.stdout.take() {
        for l in BufReader::new(o).lines().map_while(Result::ok) {
            line(&l);
        }
    }
    let errors = err.and_then(|h| h.join().ok()).unwrap_or_default();
    for l in &errors {
        line(l);
    }
    if !child.wait()?.success() {
        let why = errors
            .iter()
            .find(|l| l.trim_start().starts_with("error:"))
            .or(errors.iter().rev().find(|l| !l.trim().is_empty()))
            .map(|l| l.trim().trim_start_matches("error:").trim().to_string())
            .unwrap_or_else(|| "it stopped with an error".into());
        bail!("{program} failed: {why}");
    }
    Ok(())
}

/// `nix profile upgrade`, then the session follows (trust, units, services restarted).
/// Returns what was done and the programs still running from what it replaced.
fn profile(names: &[String], line: &mut dyn FnMut(&str)) -> Result<(String, Vec<String>)> {
    line("== Updating your Nix profile");
    let before: Vec<String> = nyx_catalog::profile().into_iter().flat_map(|e| e.store_paths).collect();
    let mut args = vec!["profile".to_string(), "upgrade".to_string()];
    if names.is_empty() {
        args.push("--all".into());
    } else {
        args.extend(names.iter().cloned());
    }
    stream("nix", &args, line)?;
    let after: Vec<String> = nyx_catalog::profile().into_iter().flat_map(|e| e.store_paths).collect();
    let replaced: Vec<String> = before.into_iter().filter(|p| !after.contains(p)).collect();
    if !replaced.is_empty() {
        nyx_catalog::after_profile_change(nyx_catalog::ProfileChange::Upgraded);
    }
    let message =
        if names.is_empty() { "Your Nix profile is up to date".into() } else { format!("Updated {}", names.join(", ")) };
    Ok((message, running_from(&replaced)))
}

/// The person's programs whose executable is in one of `paths` (replaced, so they run
/// the old version until reopened). Nothing is stopped.
pub fn running_from(paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).ok();
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let dir = entry.path();
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if uid.is_some() && std::fs::metadata(&dir).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).ok() != uid {
            continue;
        }
        let Ok(exe) = std::fs::read_link(dir.join("exe")) else { continue };
        let exe = exe.to_string_lossy();
        if paths.iter().any(|p| exe.starts_with(&format!("{p}/"))) {
            let name = exe.rsplit('/').next().unwrap_or_default();
            let name = name.trim_start_matches('.').trim_end_matches("-wrapped").trim_end_matches(" (deleted)").to_string();
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

fn flatpak(apps: &[String], line: &mut dyn FnMut(&str)) -> Result<String> {
    for a in apps {
        anyhow::ensure!(nyx_catalog::valid_flatpak(a), "not a Flatpak app ID: {a}");
    }
    line("== Updating Flatpaks");
    let mut args: Vec<String> = ["update", "--user", "--noninteractive", "-y"].map(String::from).to_vec();
    args.extend(apps.iter().cloned());
    stream("flatpak", &args, line)?;
    Ok(if apps.is_empty() { "Your Flatpaks are up to date".into() } else { format!("Updated {}", apps.join(", ")) })
}

/// A mise tool name (`node`, `github:charmbracelet/crush`) or a uv package.
fn valid_tool_name(t: &str) -> bool {
    (1..=200).contains(&t.len()) && !t.starts_with('-') && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"@:/._+-".contains(&b))
}

/// Run in the home folder, so mise upgrades what the user's own config asks for.
fn stream_at_home(program: &str, args: &[String], line: &mut dyn FnMut(&str)) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    stream_in(program, args, Some(std::path::Path::new(&home)), line)
}

fn mise(tools: &[String], line: &mut dyn FnMut(&str)) -> Result<String> {
    for t in tools {
        anyhow::ensure!(valid_tool_name(t), "not a mise tool: {t}");
    }
    line("== Updating your mise tools");
    let mut args = vec!["upgrade".to_string()];
    args.extend(tools.iter().cloned());
    stream_at_home("mise", &args, line)?;
    Ok(if tools.is_empty() { "Your mise tools are up to date".into() } else { format!("Updated {}", tools.join(", ")) })
}

fn uv(tools: &[String], line: &mut dyn FnMut(&str)) -> Result<String> {
    for t in tools {
        anyhow::ensure!(valid_tool_name(t), "not a uv tool: {t}");
    }
    line("== Updating your uv tools");
    let mut args = vec!["tool".to_string(), "upgrade".to_string()];
    if tools.is_empty() {
        args.push("--all".into());
    } else {
        args.extend(tools.iter().cloned());
    }
    stream_at_home("uv", &args, line)?;
    Ok(if tools.is_empty() { "Your uv tools are up to date".into() } else { format!("Updated {}", tools.join(", ")) })
}

fn fwupd(devices: &[String], line: &mut dyn FnMut(&str)) -> Result<String> {
    line("== Updating firmware");
    for d in devices {
        anyhow::ensure!(!d.is_empty() && d.len() <= 64 && d.bytes().all(|b| b.is_ascii_alphanumeric()), "not a device id: {d}");
        // fwupd asks polkit itself; the restart is left to the person.
        stream("fwupdmgr", &["update".into(), "--assume-yes".into(), "--no-reboot-check".into(), d.clone()], line)?;
    }
    Ok(format!("Firmware updated on {} device{}", devices.len(), if devices.len() == 1 { "" } else { "s" }))
}

/// Tell the person what's available, once per set of updates.
pub async fn notify_available(status: &crate::Status) -> Result<bool> {
    let state = user_history().with_file_name("update-notified.json");
    let fingerprint: Vec<String> =
        status.available.iter().map(|i| format!("{} {}", i.name, i.to.as_deref().unwrap_or("?"))).collect();
    let before: Vec<String> =
        std::fs::read_to_string(&state).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if fingerprint.is_empty() || fingerprint == before {
        if fingerprint.is_empty() {
            let _ = std::fs::remove_file(&state);
        }
        return Ok(false);
    }
    let shown: Vec<String> = status
        .available
        .iter()
        .take(5)
        .map(|i| match &i.to {
            Some(v) => format!("{} {v}", i.name),
            None => i.name.clone(),
        })
        .collect();
    let more = status.available.len().saturating_sub(shown.len());
    let body = format!(
        "{}{}\nRun nyx update, or open Settings > Updates.",
        shown.join(", "),
        if more > 0 { format!(" and {more} more") } else { String::new() }
    );
    let summary = if status.available.len() == 1 { "An update is available" } else { "Updates are available" };
    notify(summary, &body).await?;
    if let Some(dir) = state.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&state, serde_json::to_string(&fingerprint)?)?;
    Ok(true)
}

/// A notification from NyxOS's updates, opening Settings when clicked.
pub async fn notify(summary: &str, body: &str) -> Result<()> {
    let conn = zbus::Connection::session().await.context("can't reach the session bus")?;
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value> =
        std::collections::HashMap::from([("desktop-entry", zbus::zvariant::Value::from("io.github.design_nexus.nyx.Settings"))]);
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("NyxOS", 0u32, "system-software-update", summary, body, Vec::<&str>::new(), hints, -1i32),
    )
    .await?;
    Ok(())
}

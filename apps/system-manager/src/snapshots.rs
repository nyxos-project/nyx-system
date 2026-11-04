//! Btrfs snapshots before and after a system change, when NyxOS has them on
//! (`/etc/nyxos/snapshots.toml`, from `nyxos.snapshots`). A snapshot that can't be taken
//! never stops the change.

use crate::Manager;
use zbus::object_server::SignalEmitter;

const CONFIG: &str = "/etc/nyxos/snapshots.toml";

#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    around_changes: Vec<String>,
}

fn configs() -> Vec<String> {
    std::fs::read_to_string(CONFIG)
        .ok()
        .and_then(|t| toml::from_str::<Config>(&t).ok())
        .unwrap_or_default()
        .around_changes
        .into_iter()
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)))
        .collect()
}

async fn snapper(args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("snapper").args(args).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The "pre" snapshots: (config, number) for each that was taken.
pub async fn before(what: &str, emitter: &SignalEmitter<'_>) -> Vec<(String, String)> {
    let mut taken = Vec::new();
    for c in configs() {
        let desc = format!("Before: {what}");
        match snapper(&[
            "-c",
            &c,
            "create",
            "--type",
            "pre",
            "--print-number",
            "--cleanup-algorithm",
            "number",
            "--description",
            &desc,
        ])
        .await
        {
            Some(n) if n.chars().all(|d| d.is_ascii_digit()) && !n.is_empty() => {
                let _ = Manager::progress(emitter, &format!("Snapshot {n} of {c} taken before the change")).await;
                taken.push((c, n));
            }
            _ => {
                let _ = Manager::progress(emitter, &format!("No snapshot of {c} could be taken; going on without one")).await;
            }
        }
    }
    taken
}

/// The matching "post" snapshots.
pub async fn after(pre: Vec<(String, String)>, what: &str, emitter: &SignalEmitter<'_>) {
    for (c, n) in pre {
        let desc = format!("After: {what}");
        if snapper(&[
            "-c",
            &c,
            "create",
            "--type",
            "post",
            "--pre-number",
            &n,
            "--cleanup-algorithm",
            "number",
            "--description",
            &desc,
        ])
        .await
        .is_none()
        {
            let _ = Manager::progress(emitter, &format!("The snapshot of {c} after the change couldn't be taken")).await;
        }
    }
}

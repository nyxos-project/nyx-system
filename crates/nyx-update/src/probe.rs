//! Looking at the machine ([`Snapshot`]): the channel the system's flake follows and the
//! revision it's locked to, the tools it was built with, the catalog's installed apps,
//! the user's Nix profile and Flatpaks, and fwupd's devices. With `check`, also what's
//! available: the head of the channel (and its tool pins), Flatpak updates and firmware.

use crate::{Channel, ChannelApp, Device, Firmware, Flatpak, ProfileElement, Rev, Snapshot};
use anyhow::{Context, Result, bail};
use nyx_catalog::{Catalog, Installed};
use nyx_system_proto::{SystemSettings, TOOLS_FILE, ToolPin};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The catalog sections whose profile flakes count as tools.
const TOOL_CATEGORIES: &[&str] = &["coding-agents", "agent-apps"];

/// What to look at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Ask the network what's available.
    pub check: bool,
    /// Ask fwupd too.
    pub firmware: bool,
}

pub fn probe(opts: Options) -> Snapshot {
    let config = system_config();
    let flake = config.as_ref().map(|c| c.flake.clone()).unwrap_or_else(|| PathBuf::from("/etc/nyxos/flake"));
    let overrides = config.as_ref().map(|c| c.overrides.clone()).unwrap_or_else(|| PathBuf::from("/etc/nyxos/overrides"));
    let mut channel = locked_channel(&flake.join("flake.lock"))
        .unwrap_or_else(|e| Channel { error: Some(format!("{e:#}")), ..Channel::default() });
    let tools = read_pins(Path::new(TOOLS_FILE)).unwrap_or_default();
    let mut head_tools = None;
    if opts.check {
        match channel.url.clone().map(|u| head(&u)) {
            Some(Ok((rev, pins))) => {
                channel.head = Some(rev);
                head_tools = pins;
            }
            Some(Err(e)) => channel.error = Some(format!("{e:#}")),
            None => {}
        }
    }

    let catalog = Catalog::load().ok();
    let settings = std::fs::read_to_string(overrides.join("system.json"))
        .ok()
        .and_then(|t| SystemSettings::parse(&t).ok())
        .unwrap_or_default();
    let installed = Installed::probe(settings);
    let mut tool_aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut apps = Vec::new();
    let mut flatpak_aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut profile_items: BTreeMap<String, (bool, Vec<String>)> = BTreeMap::new();
    if let Some(c) = &catalog {
        for item in c.items.iter().filter(|i| installed.has(i)) {
            let mut names = vec![item.id.clone()];
            names.extend(item.bin.clone());
            if let Some(f) = &item.flatpak {
                flatpak_aliases.entry(f.clone()).or_default().extend(names);
            } else if let Some(url) = &item.profile {
                profile_items.insert(url.clone(), (TOOL_CATEGORIES.contains(&item.category.as_str()), names));
            } else if !item.nyx.is_empty() {
                names.extend(item.nyx.iter().cloned());
                apps.push(ChannelApp { id: item.id.clone(), name: item.name.clone(), aliases: names });
            } else if !item.packages.is_empty() && item.packages.iter().all(|p| tools.contains_key(p)) {
                for p in &item.packages {
                    tool_aliases.entry(p.clone()).or_default().extend(names.iter().filter(|n| *n != p).cloned());
                }
            } else if !item.packages.is_empty() {
                names.extend(item.packages.iter().cloned());
                apps.push(ChannelApp { id: item.id.clone(), name: item.name.clone(), aliases: names });
            }
        }
    }
    // Nyx apps the system has that no catalog item names.
    for name in &installed.nyx_apps {
        if !apps.iter().any(|a| a.aliases.contains(name)) {
            apps.push(ChannelApp {
                id: format!("nyx-{name}"),
                name: format!("Nyx {}", title(name)),
                aliases: vec![name.clone()],
            });
        }
    }

    let profile = nyx_catalog::profile_elements()
        .into_iter()
        .map(|(name, url)| {
            let (tool, aliases) = profile_items.get(&url).cloned().unwrap_or_default();
            ProfileElement { name, url, tool, aliases }
        })
        .collect();

    let mut flatpaks = user_flatpaks();
    if opts.check && !flatpaks.is_empty() {
        let updates = flatpak_updates();
        for f in &mut flatpaks {
            f.update = updates.get(&f.id).cloned().map(|v| if v.is_empty() { "newer".to_string() } else { v });
        }
    }
    for f in &mut flatpaks {
        f.aliases.extend(flatpak_aliases.remove(&f.id).unwrap_or_default());
    }

    let firmware = if opts.check && opts.firmware { firmware() } else { Firmware::NotChecked };

    Snapshot { channel, tools, head_tools, tool_aliases, apps, profile, flatpaks, firmware, checked: opts.check }
}

fn title(name: &str) -> String {
    name.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `/etc/nyxos/system.toml`, the System Manager's: where the flake and overrides are.
#[derive(Debug, Deserialize)]
struct SystemConfig {
    flake: PathBuf,
    #[serde(default = "overrides")]
    overrides: PathBuf,
}

fn overrides() -> PathBuf {
    "/etc/nyxos/overrides".into()
}

fn system_config() -> Option<SystemConfig> {
    let path = std::env::var_os("NYX_SYSTEM_CONFIG").map(PathBuf::from).unwrap_or_else(|| "/etc/nyxos/system.toml".into());
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The flake's `nyxos` input as its lock has it: the channel URL and the locked revision.
pub fn locked_channel(lock: &Path) -> Result<Channel> {
    let text = std::fs::read_to_string(lock).with_context(|| format!("can't read {}", lock.display()))?;
    parse_lock(&text)
}

fn parse_lock(text: &str) -> Result<Channel> {
    let v: serde_json::Value = serde_json::from_str(text).context("flake.lock isn't JSON")?;
    let root = v["root"].as_str().unwrap_or("root");
    let node_name = match &v["nodes"][root]["inputs"]["nyxos"] {
        serde_json::Value::String(s) => s.clone(),
        _ => bail!("this system's flake has no nyxos input"),
    };
    let node = &v["nodes"][node_name.as_str()];
    let original = &node["original"];
    let locked = &node["locked"];
    if original["type"].as_str() != Some("github") {
        bail!("the nyxos input isn't a GitHub channel, so there's no channel to check");
    }
    let (owner, repo) = (original["owner"].as_str().unwrap_or_default(), original["repo"].as_str().unwrap_or_default());
    let branch = original["ref"].as_str().unwrap_or("main").to_string();
    let url = format!("github:{owner}/{repo}/{branch}");
    let installed =
        locked["rev"].as_str().map(|r| Rev { rev: r.to_string(), last_modified: locked["lastModified"].as_i64().unwrap_or(0) });
    Ok(Channel { url: Some(url), branch, installed, head: None, error: None })
}

/// Where a channel keeps its tool pins.
pub const SOURCES: &str = "pkgs/tools/sources.json";

/// The head of the channel `url` (`github:owner/repo/branch`), fetched into the store (a
/// small source download): its revision and where its source is.
pub fn fetch_head(url: &str) -> Result<(Rev, PathBuf)> {
    let out = Command::new("nix").args(["flake", "prefetch", "--json", "--refresh", url]).output().context("can't run nix")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().find(|l| l.trim_start().starts_with("error:")).unwrap_or(err.lines().last().unwrap_or(""));
        bail!("{}", why.trim().trim_start_matches("error:").trim());
    }
    parse_prefetch(&String::from_utf8_lossy(&out.stdout))
}

fn head(url: &str) -> Result<(Rev, Option<BTreeMap<String, ToolPin>>)> {
    let (rev, path) = fetch_head(url)?;
    Ok((rev, read_pins(&path.join(SOURCES)).ok()))
}

fn parse_prefetch(json: &str) -> Result<(Rev, PathBuf)> {
    let v: serde_json::Value = serde_json::from_str(json).context("nix flake prefetch didn't answer JSON")?;
    let rev = v["locked"]["rev"].as_str().context("the channel's head has no revision")?.to_string();
    let last_modified = v["locked"]["lastModified"].as_i64().unwrap_or(0);
    let path = v["storePath"].as_str().context("nix flake prefetch gave no store path")?;
    Ok((Rev { rev, last_modified }, PathBuf::from(path)))
}

/// Tool pins from a sources.json or [`TOOLS_FILE`].
pub fn read_pins(path: &Path) -> Result<BTreeMap<String, ToolPin>> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn lines(cmd: &mut Command) -> Vec<String> {
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect())
        .unwrap_or_default()
}

fn user_flatpaks() -> Vec<Flatpak> {
    lines(Command::new("flatpak").args(["list", "--user", "--app", "--columns=application,name,version"]))
        .iter()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim().to_string();
            let name = f.next().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&id).to_string();
            let version = f.next().map(str::trim).filter(|v| !v.is_empty()).map(String::from);
            let short = id.rsplit('.').next().unwrap_or(&id).to_lowercase();
            Some(Flatpak { id, name, version, update: None, aliases: vec![short] })
        })
        .collect()
}

/// Flatpak apps with an update, and the version it brings (empty when the app doesn't say).
fn flatpak_updates() -> BTreeMap<String, String> {
    lines(Command::new("flatpak").args(["remote-ls", "--user", "--updates", "--app", "--columns=application,version"]))
        .iter()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?.trim().to_string(), f.next().unwrap_or("").trim().to_string()))
        })
        .collect()
}

/// fwupd's devices that have an update, or why it can't be asked.
fn firmware() -> Firmware {
    let out = match Command::new("fwupdmgr").args(["get-updates", "--json"]).output() {
        Ok(o) => o,
        Err(_) => {
            return Firmware::Unavailable { reason: "fwupd isn't installed (nyx-system service fwupd on turns it on)".into() };
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    if text.trim().is_empty() {
        if err.contains("daemon") || err.contains("connect") {
            return Firmware::Unavailable { reason: "fwupd isn't running (nyx-system service fwupd on turns it on)".into() };
        }
        // "No updatable devices" / "No updates available" (exit status 2).
        return Firmware::Devices { devices: Vec::new() };
    }
    match parse_fwupd(&text) {
        Ok(devices) => Firmware::Devices { devices },
        Err(e) => Firmware::Unavailable { reason: format!("fwupd's answer couldn't be read: {e:#}") },
    }
}

fn parse_fwupd(json: &str) -> Result<Vec<Device>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Answer {
        #[serde(default)]
        devices: Vec<Dev>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Dev {
        name: String,
        device_id: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        releases: Vec<Release>,
        #[serde(default)]
        flags: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Release {
        version: String,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        flags: Vec<String>,
    }
    let a: Answer = serde_json::from_str(json)?;
    Ok(a.devices
        .into_iter()
        .map(|d| {
            let r = d.releases.into_iter().next();
            let needs_restart = d.flags.iter().chain(r.iter().flat_map(|r| &r.flags)).any(|f| f.contains("needs-reboot"));
            Device {
                id: d.device_id,
                name: d.name,
                version: d.version,
                update: r.as_ref().map(|r| r.version.clone()),
                summary: r.and_then(|r| r.summary),
                needs_restart,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_names_the_channel() {
        let lock = r#"{
          "nodes": {
            "nyxos": {
              "locked": {"lastModified": 1793836800, "owner": "nyxos-project", "repo": "nyxos", "rev": "434e34eabc", "type": "github"},
              "original": {"owner": "nyxos-project", "ref": "stable", "repo": "nyxos", "type": "github"}
            },
            "root": {"inputs": {"nyxos": "nyxos", "nyxos-overrides": "nyxos-overrides"}}
          },
          "root": "root", "version": 7
        }"#;
        let c = parse_lock(lock).unwrap();
        assert_eq!(c.url.as_deref(), Some("github:nyxos-project/nyxos/stable"));
        assert_eq!(c.branch, "stable");
        assert_eq!(c.installed, Some(Rev { rev: "434e34eabc".into(), last_modified: 1_793_836_800 }));
        assert!(parse_lock(r#"{"nodes": {"root": {"inputs": {}}}, "root": "root"}"#).is_err());
    }

    #[test]
    fn the_head_brings_its_pins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkgs/tools")).unwrap();
        std::fs::write(
            dir.path().join("pkgs/tools/sources.json"),
            r#"{"claude-code": {"name": "Claude Code", "version": "2.1.311", "recipe": 1, "aliases": ["claude"], "srcs": {}, "upstream": {}}}"#,
        )
        .unwrap();
        let json = format!(
            r#"{{"hash": "sha256-x", "locked": {{"rev": "9f00ba1", "lastModified": 5}}, "storePath": "{}"}}"#,
            dir.path().display()
        );
        let (rev, path) = parse_prefetch(&json).unwrap();
        assert_eq!(rev.rev, "9f00ba1");
        assert_eq!(read_pins(&path.join(SOURCES)).unwrap()["claude-code"].version, "2.1.311");
    }

    #[test]
    fn fwupd_devices_with_releases() {
        let json = r#"{"Devices": [{"Name": "System Firmware", "DeviceId": "abc", "Version": "1.10",
            "Flags": ["internal", "updatable"],
            "Releases": [{"Version": "1.12", "Summary": "Fixes", "Flags": ["is-upgrade", "needs-reboot"]}]}]}"#;
        let d = parse_fwupd(json).unwrap();
        assert_eq!(d[0].update.as_deref(), Some("1.12"));
        assert!(d[0].needs_restart);
        assert_eq!(title("media-player"), "Media Player");
    }
}

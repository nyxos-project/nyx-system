//! Looking at the machine ([`Snapshot`]): the channel the system's flake follows and the
//! revision it's locked to, the tools it was built with, the catalog's installed apps,
//! the user's Nix profile and Flatpaks, and fwupd's devices. With `check`, also what's
//! available: the head of the channel (and its tool pins), Flatpak updates and firmware.

use crate::{Channel, ChannelApp, Device, Firmware, Flatpak, ProfileElement, Rev, Snapshot, UserTool};
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
    // Catalog items by the package a profile installs them as: whether it's a tool, and
    // the names it answers to.
    let mut by_attr: BTreeMap<String, (bool, Vec<String>, String)> = BTreeMap::new();
    if let Some(c) = &catalog {
        for item in &c.items {
            let tool = TOOL_CATEGORIES.contains(&item.category.as_str());
            for attr in item.profile_attrs() {
                by_attr.insert(attr, (tool, item.names(), item.name.clone()));
            }
        }
        for item in c.items.iter().filter(|i| installed.has(i)) {
            let mut names = vec![item.id.clone()];
            names.extend(item.bin.clone());
            let attrs = item.profile_attrs();
            // Only in the person's profile: its profile element is what updates.
            let only_in_profile = !attrs.is_empty()
                && attrs.iter().all(|a| installed.profile_attrs.contains(a))
                && !installed.system_wide(item)
                && !item.packages.iter().any(|p| tools.get(p).is_some_and(|t| t.installed));
            if only_in_profile {
                continue;
            }
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

    let elements = installed.profile.clone();
    let mut profile: Vec<ProfileElement> = elements
        .iter()
        .map(|e| {
            let (tool, aliases, title) = match e.package().and_then(|a| by_attr.get(a)) {
                Some((tool, names, title)) => (*tool, names.clone(), Some(title.clone())),
                None => {
                    let (tool, names) = profile_items.get(&e.reference()).cloned().unwrap_or_default();
                    (tool, names, None)
                }
            };
            ProfileElement {
                name: e.name.clone(),
                url: e.reference(),
                tool,
                aliases,
                version: e.version(),
                title,
                ..ProfileElement::default()
            }
        })
        .collect();
    if opts.check
        && let (Some(url), Some(head)) = (&channel.url, &channel.head)
    {
        channel_versions(url, head, &elements, head_tools.as_ref(), &mut profile);
    }

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
    let (mise, uv_tools) = (mise_tools(opts.check), uv_tools(opts.check));

    Snapshot { channel, tools, head_tools, tool_aliases, apps, profile, flatpaks, firmware, mise, uv_tools, checked: opts.check }
}

/// What the head of the channel has for the profile's packages that come from the
/// channel (`url`): a tool's pin, or the package's version in the channel's packages
/// (one evaluation at the head's revision, so it's what `nix profile upgrade` gets).
fn channel_versions(
    url: &str,
    head: &Rev,
    elements: &[nyx_catalog::ProfileElement],
    pins: Option<&BTreeMap<String, ToolPin>>,
    profile: &mut [ProfileElement],
) {
    let from_channel: Vec<(usize, String)> = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.url == url)
        .filter_map(|(i, e)| e.package().filter(|a| nyx_system_proto::valid_package(a)).map(|a| (i, a.to_string())))
        .collect();
    let mut newest: BTreeMap<usize, String> = BTreeMap::new();
    let mut ask = Vec::new();
    for (i, attr) in &from_channel {
        match pins.and_then(|p| p.get(attr)) {
            Some(pin) => {
                newest.insert(*i, pin.version.clone());
            }
            None => ask.push((*i, attr.clone())),
        }
    }
    if !ask.is_empty()
        && let Some((base, _)) = url.rsplit_once('/')
    {
        let list: Vec<String> = ask.iter().map(|(_, a)| format!("\"{a}\"")).collect();
        let apply =
            format!("p: map (a: (p.lib.attrByPath (p.lib.splitString \".\" a) {{ }} p).version or null) [ {} ]", list.join(" "));
        let flake = format!("{base}/{}#legacyPackages.{}-linux", head.rev, std::env::consts::ARCH);
        let versions: Vec<Option<String>> = Command::new("nix")
            .args(["eval", "--json", &flake, "--apply", &apply])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice(&o.stdout).ok())
            .unwrap_or_default();
        for ((i, _), v) in ask.iter().zip(versions) {
            if let Some(v) = v {
                newest.insert(*i, v);
            }
        }
    }
    for (i, version) in newest {
        let p = &mut profile[i];
        p.known = true;
        let older = p
            .version
            .as_deref()
            .is_none_or(|have| nyx_system_proto::compare_versions(have, &version) == std::cmp::Ordering::Less);
        if older {
            p.update = Some(version);
        }
    }
}

/// A command's output, run in the home folder (so mise reads the user's own config, not
/// a project's), or nothing when it isn't installed or fails.
fn home_output(program: &str, args: &[&str]) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let out = Command::new(program).args(args).current_dir(home).stdin(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The tools the user's mise config asks for and has installed, and with `check`, the
/// newer versions that config allows.
fn mise_tools(check: bool) -> Vec<UserTool> {
    let Some(ls) = home_output("mise", &["ls", "--current", "--json"]) else { return Vec::new() };
    let outdated = if check { home_output("mise", &["outdated", "--json"]).unwrap_or_default() } else { String::new() };
    parse_mise(&ls, &outdated)
}

fn parse_mise(ls: &str, outdated: &str) -> Vec<UserTool> {
    #[derive(Deserialize)]
    struct Installed {
        version: String,
        #[serde(default)]
        installed: bool,
    }
    #[derive(Deserialize)]
    struct Outdated {
        current: Option<String>,
        latest: Option<String>,
    }
    let installed: BTreeMap<String, Vec<Installed>> = serde_json::from_str(ls).unwrap_or_default();
    let outdated: BTreeMap<String, Outdated> = serde_json::from_str(outdated).unwrap_or_default();
    installed
        .into_iter()
        .filter_map(|(name, versions)| {
            let version = versions.into_iter().find(|v| v.installed)?.version;
            let update =
                outdated.get(&name).filter(|o| o.current.is_some() && o.latest != o.current).and_then(|o| o.latest.clone());
            // `github:charmbracelet/crush` answers to `crush` too.
            let short = name.rsplit(['/', ':']).next().unwrap_or(&name).to_string();
            let aliases = if short == name { Vec::new() } else { vec![short] };
            Some(UserTool { name, version: Some(version), update, aliases })
        })
        .collect()
}

/// `uv tool list` (with `--outdated`, the newer version each has).
fn uv_tools(check: bool) -> Vec<UserTool> {
    let mut tools = home_output("uv", &["tool", "list"]).map(|t| parse_uv(&t)).unwrap_or_default();
    if check && !tools.is_empty() {
        let newer = home_output("uv", &["tool", "list", "--outdated", "--show-version-specifiers"])
            .map(|t| parse_uv(&t))
            .unwrap_or_default();
        for t in &mut tools {
            t.update = newer.iter().find(|n| n.name == t.name).and_then(|n| n.update.clone());
        }
    }
    tools
}

/// `name v1.2 [required: >=1] [latest: 1.3]` lines, each followed by `- command` lines.
/// uv upgrades a tool only within what it was installed with: one installed `==1.2`
/// has no update to offer, and one with a range gets the newest only if it fits.
fn parse_uv(text: &str) -> Vec<UserTool> {
    let mut out: Vec<UserTool> = Vec::new();
    for line in text.lines() {
        if let Some(cmd) = line.strip_prefix("- ") {
            if let Some(t) = out.last_mut()
                && *cmd != t.name
            {
                t.aliases.push(cmd.trim().to_string());
            }
            continue;
        }
        let mut words = line.splitn(3, ' ');
        let (Some(name), Some(version)) = (words.next(), words.next()) else { continue };
        let Some(version) = version.strip_prefix('v') else { continue };
        let rest = words.next().unwrap_or("");
        let tag = |key: &str| {
            let start = rest.find(&format!("[{key}: "))? + key.len() + 3;
            Some(rest[start..].split(']').next()?.trim().to_string())
        };
        let update = match (tag("latest"), tag("required")) {
            (Some(_), Some(spec)) if spec.starts_with("==") => None,
            (Some(latest), Some(spec)) => Some(format!("{latest} (if {spec} allows)")),
            (latest, None) => latest,
            (None, _) => None,
        };
        out.push(UserTool { name: name.to_string(), version: Some(version.to_string()), update, aliases: Vec::new() });
    }
    out
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
    fn mise_tools_and_their_news() {
        let ls = r#"{
          "node": [{"version": "22.1.0", "requested_version": "22", "installed": true, "active": true}],
          "github:charmbracelet/crush": [{"version": "0.99.0", "installed": true}],
          "amp": [{"version": "0.0.1", "installed": false}]
        }"#;
        let outdated = r#"{
          "github:charmbracelet/crush": {"name": "github:charmbracelet/crush", "requested": "latest", "current": "0.99.0", "latest": "0.100.0"},
          "amp": {"current": null, "latest": "0.0.2"}
        }"#;
        let t = parse_mise(ls, outdated);
        assert_eq!(t.len(), 2, "amp isn't installed");
        let crush = t.iter().find(|t| t.name == "github:charmbracelet/crush").unwrap();
        assert_eq!((crush.update.as_deref(), crush.aliases.as_slice()), (Some("0.100.0"), &["crush".to_string()][..]));
        assert!(t.iter().any(|t| t.name == "node" && t.update.is_none() && t.aliases.is_empty()));
        assert!(parse_mise("not json", "").is_empty());
    }

    #[test]
    fn uv_tools_and_their_news() {
        let text = "browser-use v0.13.10 [latest: 0.13.14]\n- browser\n- browser-use\n- bu\ntrafilatura v2.3.0\n- trafilatura\n";
        let t = parse_uv(text);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].update.as_deref(), Some("0.13.14"));
        assert_eq!(t[0].aliases, vec!["browser", "bu"]);
        assert_eq!((t[1].version.as_deref(), t[1].update.as_deref()), (Some("2.3.0"), None));
        assert!(t[1].aliases.is_empty());
        let held = parse_uv(
            "pycowsay v0.0.0.1 [required: ==0.0.0.1] [latest: 0.0.0.2]\n- pycowsay\nruff v0.9.0 [required: >=0.9,<1] [latest: 1.2.0]\n",
        );
        assert_eq!(held[0].update, None, "uv won't move a tool installed ==");
        assert_eq!(held[1].update.as_deref(), Some("1.2.0 (if >=0.9,<1 allows)"));
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

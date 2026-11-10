//! `nyx update`: everything on NyxOS that can be updated, found by name, planned, then
//! updated through the one mechanism that owns it.
//!
//! - The **channel** (NixOS, the NyxOS modules, nixpkgs, every Nyx app) through the
//!   System Manager, which updates the system's flake and rebuilds.
//! - **Tools** NyxOS packages from pins (coding agents and the like), one at a time, by
//!   the System Manager copying a tool's pin from the head of the channel.
//! - The user's **Nix profile** (`nix profile upgrade`), **Flatpaks**
//!   (`flatpak update --user`), **mise** tools (`mise upgrade`) and **uv** tools
//!   (`uv tool upgrade`), as the user.
//! - **Firmware** through fwupd, never without asking.
//!
//! [`Snapshot`] is what's on the machine and what's available ([`probe`] looks);
//! [`targets`] turns it into named [`Target`]s; [`resolve`] finds what the person asked
//! for; [`plan`] says what updating it takes; [`run`] does it. Settings and the Store use
//! the same engine through `nyx-system update --json`.

pub mod cache;
pub mod policy;
pub mod probe;
pub mod run;

use anyhow::Result;
use nyx_system_proto::ToolPin;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The reserved names: `nyx update system` and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    System,
    Apps,
    Tools,
    Firmware,
}

impl Category {
    pub const ALL: [Category; 4] = [Category::System, Category::Apps, Category::Tools, Category::Firmware];

    pub fn parse(s: &str) -> Option<Category> {
        Some(match s {
            "system" => Category::System,
            "apps" => Category::Apps,
            "tools" => Category::Tools,
            "firmware" => Category::Firmware,
            _ => return None,
        })
    }

    pub fn title(self) -> &'static str {
        match self {
            Category::System => "System",
            Category::Apps => "Applications",
            Category::Tools => "Tools",
            Category::Firmware => "Firmware",
        }
    }
}

/// The mechanism that owns a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// The system channel: NixOS, Nyx apps, apps from nixpkgs.
    Channel,
    /// A tool NyxOS packages from a pin.
    ToolPin,
    /// The user's Nix profile.
    Profile,
    Flatpak,
    Firmware,
    /// A tool or language the user installed with mise (their global mise config).
    Mise,
    /// A Python tool the user installed with `uv tool install`.
    UvTool,
}

/// What finishing an update of it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Restart {
    /// Reopening the app, if it's open.
    App,
    /// Maybe restarting the computer (a new kernel, new firmware).
    Computer,
}

/// Something that can be updated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    /// What `nyx update ID` takes: a pin's name, a catalog id, a profile element, a
    /// Flatpak id.
    pub id: String,
    pub name: String,
    pub category: Category,
    pub provider: Provider,
    pub installed: Option<String>,
    /// A newer version, when one is known.
    pub available: Option<String>,
    /// Other names it answers to.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The administrator's password (a system rebuild).
    pub admin: bool,
    pub restart: Restart,
    /// Whether a mistake can be undone (a generation, a profile rollback).
    pub rollback: bool,
    /// A tool whose newer pin is built differently now: it comes with the system update.
    #[serde(default)]
    pub needs_channel: bool,
    /// Whether a newer version could be known (it's checked only when it updates: a
    /// profile package from outside the system's channel).
    #[serde(default = "yes")]
    pub checkable: bool,
    /// The provider's own handle: a firmware device id, a profile element, a Flatpak id.
    pub key: String,
}

impl Target {
    fn answers_to(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.id.to_lowercase() == n || self.name.to_lowercase() == n || self.aliases.iter().any(|a| a.to_lowercase() == n)
    }

    pub fn has_update(&self) -> bool {
        self.available.is_some()
    }
}

fn yes() -> bool {
    true
}

/// A channel revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rev {
    pub rev: String,
    /// Seconds since the epoch.
    pub last_modified: i64,
}

impl Rev {
    pub fn describe(&self) -> String {
        format!("{} ({})", &self.rev[..self.rev.len().min(7)], date(self.last_modified))
    }
}

/// `YYYY-MM-DD` for seconds since the epoch (UTC).
pub fn date(secs: i64) -> String {
    // Howard Hinnant's civil-from-days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The system's channel: the flake input `nyxos` and its branch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    /// `github:nyxos-project/nyxos/stable`.
    pub url: Option<String>,
    /// `stable`.
    pub branch: String,
    pub installed: Option<Rev>,
    /// The head of the branch, once checked.
    pub head: Option<Rev>,
    /// Why it couldn't be checked.
    pub error: Option<String>,
}

impl Channel {
    pub fn has_update(&self) -> bool {
        matches!((&self.installed, &self.head), (Some(i), Some(h)) if i.rev != h.rev && h.last_modified >= i.last_modified)
    }
}

/// An app that comes with the channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelApp {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
}

/// An element of the user's Nix profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileElement {
    pub name: String,
    pub url: String,
    /// A coding agent (from the catalog's agent sections) counts as a tool.
    pub tool: bool,
    pub aliases: Vec<String>,
    /// The version installed, from its store path.
    #[serde(default)]
    pub version: Option<String>,
    /// The version the channel's head has, once checked.
    #[serde(default)]
    pub update: Option<String>,
    /// Whether the newest version is known (it comes from the system's channel and
    /// was checked); otherwise only `nix profile upgrade` finds out.
    #[serde(default)]
    pub known: bool,
    /// The catalog's name for it (`Claude Code`), when it's a catalog item.
    #[serde(default)]
    pub title: Option<String>,
}

impl ProfileElement {
    /// Whether upgrading it may bring something: news, or nobody knows.
    pub fn may_update(&self) -> bool {
        self.update.is_some() || !self.known
    }
}

/// A Flatpak installed for the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flatpak {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    /// The version an update brings, once checked.
    pub update: Option<String>,
    pub aliases: Vec<String>,
}

/// A tool the user installed with mise or uv.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTool {
    /// mise's name for it (`node`, `github:charmbracelet/crush`), or the uv package.
    pub name: String,
    pub version: Option<String>,
    /// The version an update brings, once checked.
    pub update: Option<String>,
    /// Its short name and the commands it installs.
    pub aliases: Vec<String>,
}

/// A device fwupd can update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub version: String,
    pub update: Option<String>,
    /// What the release says about itself.
    pub summary: Option<String>,
    pub needs_restart: bool,
}

/// What fwupd says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Firmware {
    /// Not checked (not asked for, or turned off in the update settings).
    NotChecked,
    /// fwupd isn't there or isn't running: why, and what to do.
    Unavailable {
        reason: String,
    },
    Devices {
        devices: Vec<Device>,
    },
}

/// What's on this machine and what's available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub channel: Channel,
    /// The tools as this system was built with them ([`nyx_system_proto::TOOLS_FILE`]).
    pub tools: BTreeMap<String, ToolPin>,
    /// The pins at the head of the channel, once checked.
    pub head_tools: Option<BTreeMap<String, ToolPin>>,
    /// More names for tools, from the catalog (`cursor-agent` for `cursor-cli`).
    pub tool_aliases: BTreeMap<String, Vec<String>>,
    pub apps: Vec<ChannelApp>,
    pub profile: Vec<ProfileElement>,
    pub flatpaks: Vec<Flatpak>,
    pub firmware: Firmware,
    /// What the user installed with mise, and with `uv tool install`.
    #[serde(default)]
    pub mise: Vec<UserTool>,
    #[serde(default)]
    pub uv_tools: Vec<UserTool>,
    /// Whether availability was checked (over the network).
    pub checked: bool,
}

/// Everything in `s` that can be updated.
pub fn targets(s: &Snapshot) -> Vec<Target> {
    let mut out = Vec::new();
    out.push(Target {
        id: "system".into(),
        name: format!("NyxOS ({})", if s.channel.branch.is_empty() { "channel" } else { &s.channel.branch }),
        category: Category::System,
        provider: Provider::Channel,
        installed: s.channel.installed.as_ref().map(Rev::describe),
        available: s.channel.has_update().then(|| s.channel.head.as_ref().map(Rev::describe)).flatten(),
        aliases: vec!["nyxos".into(), "nixos".into()],
        admin: true,
        restart: Restart::Computer,
        rollback: true,
        needs_channel: false,
        checkable: true,
        key: "channel".into(),
    });
    for (id, pin) in s.tools.iter().filter(|(_, p)| p.installed) {
        let head = s.head_tools.as_ref().and_then(|h| h.get(id));
        let newer = head.filter(|h| pin.older_than(h));
        let mut aliases = pin.aliases.clone();
        aliases.extend(s.tool_aliases.get(id).cloned().unwrap_or_default());
        out.push(Target {
            id: id.clone(),
            name: if pin.name.is_empty() { id.clone() } else { pin.name.clone() },
            category: Category::Tools,
            provider: Provider::ToolPin,
            installed: Some(pin.release()),
            available: newer.map(ToolPin::release),
            aliases,
            admin: true,
            restart: Restart::App,
            rollback: true,
            needs_channel: newer.is_some_and(|h| h.recipe != pin.recipe),
            checkable: true,
            key: id.clone(),
        });
    }
    for a in &s.apps {
        out.push(Target {
            id: a.id.clone(),
            name: a.name.clone(),
            category: Category::Apps,
            provider: Provider::Channel,
            installed: None,
            available: None,
            aliases: a.aliases.clone(),
            admin: true,
            restart: Restart::App,
            rollback: true,
            needs_channel: false,
            checkable: true,
            key: "channel".into(),
        });
    }
    for e in &s.profile {
        out.push(Target {
            id: e.name.clone(),
            name: e.title.clone().unwrap_or_else(|| e.name.clone()),
            category: if e.tool { Category::Tools } else { Category::Apps },
            provider: Provider::Profile,
            installed: e.version.clone(),
            available: e.update.clone(),
            aliases: e.aliases.clone(),
            admin: false,
            restart: Restart::App,
            rollback: true,
            needs_channel: false,
            checkable: e.known,
            key: e.name.clone(),
        });
    }
    for f in &s.flatpaks {
        out.push(Target {
            id: f.id.clone(),
            name: f.name.clone(),
            category: Category::Apps,
            provider: Provider::Flatpak,
            installed: f.version.clone(),
            available: f.update.clone(),
            aliases: f.aliases.clone(),
            admin: false,
            restart: Restart::App,
            rollback: false,
            needs_channel: false,
            checkable: true,
            key: f.id.clone(),
        });
    }
    for (provider, tools) in [(Provider::Mise, &s.mise), (Provider::UvTool, &s.uv_tools)] {
        for t in tools {
            out.push(Target {
                id: t.name.clone(),
                name: t.name.clone(),
                category: Category::Tools,
                provider,
                installed: t.version.clone(),
                available: t.update.clone(),
                aliases: t.aliases.clone(),
                admin: false,
                restart: Restart::App,
                rollback: false,
                needs_channel: false,
                checkable: true,
                key: t.name.clone(),
            });
        }
    }
    if let Firmware::Devices { devices } = &s.firmware {
        for d in devices.iter().filter(|d| d.update.is_some()) {
            out.push(Target {
                id: format!("firmware:{}", slug(&d.name)),
                name: d.name.clone(),
                category: Category::Firmware,
                provider: Provider::Firmware,
                installed: Some(d.version.clone()),
                available: d.update.clone(),
                aliases: Vec::new(),
                admin: false,
                restart: Restart::Computer,
                rollback: false,
                needs_channel: false,
                checkable: true,
                key: d.id.clone(),
            });
        }
    }
    out
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// What a name on the command line stands for.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Category(Category),
    /// An index into the targets.
    Target(usize),
}

/// The words `nyx update` keeps for itself: the categories and its subcommands. No
/// package is looked up under these.
pub const RESERVED: &[&str] = &["system", "apps", "tools", "firmware", "check", "status", "history", "report", "auto", "help"];

/// A name nothing installed answers to.
#[derive(Debug, Clone, PartialEq)]
pub struct Unknown {
    pub name: String,
    pub suggestions: Vec<String>,
    /// A tool NyxOS packages that isn't installed here: its name and its id.
    pub not_installed: Option<(String, String)>,
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((name, id)) = &self.not_installed {
            return write!(f, "{name} isn't installed here; nyx install {id} installs it");
        }
        if self.suggestions.is_empty() {
            write!(f, "nothing installed here is called {} (nyx update status lists what is)", self.name)
        } else {
            write!(f, "nothing installed here is called {}; did you mean {}?", self.name, self.suggestions.join(", "))
        }
    }
}

impl std::error::Error for Unknown {}

/// A name several installed things answer to: their indices into the targets.
#[derive(Debug, Clone, PartialEq)]
pub struct Ambiguous {
    pub name: String,
    pub options: Vec<usize>,
    /// Each option's id and what it is, for the person to choose from.
    pub described: Vec<(String, String)>,
}

impl std::fmt::Display for Ambiguous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list: Vec<String> = self.described.iter().map(|(id, what)| format!("  {id:<24} {what}")).collect();
        write!(f, "{} could be any of these; name the one you mean:\n{}", self.name, list.join("\n"))
    }
}

impl std::error::Error for Ambiguous {}

/// Find what each of `names` stands for: a category, or the one installed target that
/// answers to it. A name that fits none is an [`Unknown`] error, several an
/// [`Ambiguous`] one.
pub fn resolve(targets: &[Target], names: &[&str]) -> Result<Vec<Pick>> {
    if names.is_empty() {
        return Ok(Category::ALL.into_iter().map(Pick::Category).collect());
    }
    let mut picks = Vec::new();
    for name in names {
        if let Some(c) = Category::parse(name) {
            picks.push(Pick::Category(c));
            continue;
        }
        let mut found: Vec<usize> = targets.iter().enumerate().filter(|(_, t)| t.answers_to(name)).map(|(i, _)| i).collect();
        // An app that comes with the system only updates with it: when the person also has
        // their own copy of the same app (their profile), the name means that one.
        let with_system = |i: usize| targets[i].provider == Provider::Channel && targets[i].category != Category::System;
        let same = |a: usize, b: usize| targets[a].id == targets[b].id || targets[a].name == targets[b].name;
        let all = found.clone();
        found.retain(|&i| !(with_system(i) && all.iter().any(|&j| !with_system(j) && same(i, j))));
        match found.as_slice() {
            [one] => picks.push(Pick::Target(*one)),
            [] => {
                return Err(
                    Unknown { name: name.to_string(), suggestions: suggestions(targets, name), not_installed: None }.into()
                );
            }
            many => {
                let described = many
                    .iter()
                    .map(|i| {
                        let t = &targets[*i];
                        (t.id.clone(), format!("{} ({})", t.name, how(t)))
                    })
                    .collect();
                return Err(Ambiguous { name: name.to_string(), options: many.to_vec(), described }.into());
            }
        }
    }
    Ok(picks)
}

/// [`resolve`], saying so when a name is a tool NyxOS packages that isn't installed here.
pub fn resolve_installed(s: &Snapshot, targets: &[Target], names: &[&str]) -> Result<Vec<Pick>> {
    if names.is_empty() {
        return resolve(targets, names);
    }
    let mut picks = Vec::new();
    for name in names {
        match resolve(targets, &[name]) {
            Ok(p) => picks.extend(p),
            Err(e) => {
                let n = name.to_lowercase();
                let missing = s
                    .tools
                    .iter()
                    .find(|(id, p)| !p.installed && (id.as_str() == n || p.name.to_lowercase() == n || p.aliases.contains(&n)));
                match (missing, e.downcast_ref::<Unknown>()) {
                    (Some((id, p)), Some(u)) => {
                        return Err(Unknown { not_installed: Some((p.name.clone(), id.clone())), ..u.clone() }.into());
                    }
                    _ => return Err(e),
                }
            }
        }
    }
    Ok(picks)
}

/// How a target is updated, in a few words.
pub fn how(t: &Target) -> &'static str {
    match t.provider {
        Provider::Channel if t.category == Category::System => "the system channel",
        Provider::Channel => "comes with the system channel",
        Provider::ToolPin => "a NyxOS tool",
        Provider::Profile => "your Nix profile",
        Provider::Flatpak => "a Flatpak",
        Provider::Firmware => "firmware",
        Provider::Mise => "your mise tools",
        Provider::UvTool => "your uv tools",
    }
}

/// Commands both NyxOS and the user's mise or uv install: whichever comes first on the
/// PATH runs (mise's, in a shell where mise is active).
pub fn doubles(targets: &[Target]) -> Vec<String> {
    let nyxos: Vec<&Target> = targets.iter().filter(|t| t.provider == Provider::ToolPin).collect();
    let mut out = Vec::new();
    for t in targets.iter().filter(|t| matches!(t.provider, Provider::Mise | Provider::UvTool)) {
        let names: Vec<&String> = std::iter::once(&t.id).chain(&t.aliases).collect();
        if let Some(n) = nyxos.iter().find(|n| names.iter().any(|x| n.answers_to(x))) {
            out.push(format!(
                "{} is installed by both NyxOS and {}; each updates its own, and whichever is first on the PATH runs.",
                n.name,
                if t.provider == Provider::Mise { "mise" } else { "uv" }
            ));
        }
    }
    out
}

/// Installed names close to `name`.
fn suggestions(targets: &[Target], name: &str) -> Vec<String> {
    let n = name.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for t in targets {
        let names = std::iter::once(&t.id).chain(&t.aliases);
        if names.into_iter().any(|x| {
            let x = x.to_lowercase();
            (n.len() >= 3 && (x.contains(&n) || n.contains(&x) && x.len() >= 3)) || distance(&x, &n) <= 2
        }) && !out.contains(&t.id)
        {
            out.push(t.id.clone());
        }
    }
    out.truncate(5);
    out
}

/// Levenshtein distance.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// One step of a plan, for the mechanism that owns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Step {
    /// One System Manager rebuild: the channel and/or tool pins.
    System { channel: bool, tools: Vec<String> },
    /// `nix profile upgrade`: these elements, or every one when empty.
    Profile { names: Vec<String> },
    /// `flatpak update --user`: these apps, or every one when empty.
    Flatpak { apps: Vec<String> },
    /// `fwupdmgr update`: these devices (ids). Never without asking.
    Firmware { devices: Vec<String> },
    /// `mise upgrade`: these tools, or every one when empty (within what the user's mise
    /// config asks for).
    Mise { tools: Vec<String> },
    /// `uv tool upgrade`: these tools, or every one when empty.
    UvTools { tools: Vec<String> },
}

/// One line of a plan: what changes, from what to what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// How it's updated.
    pub how: String,
}

/// What updating a request takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub request: String,
    pub steps: Vec<Step>,
    pub items: Vec<Item>,
    /// What the person should know: what comes with what, what's already current.
    pub notes: Vec<String>,
    /// Something asked for needs the system update instead: offer it.
    pub suggest_system: bool,
    /// The administrator's password is asked for (a system rebuild).
    pub admin: bool,
    /// A restart of the computer may be needed afterwards.
    pub may_restart: bool,
    /// The firmware step was only implied (`nyx update`): ask before it, and leave it
    /// out of an unattended run.
    pub firmware_implied: bool,
    /// How long finding what's new took, for the history.
    #[serde(default)]
    pub check_seconds: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The plan for `picks`.
pub fn plan(s: &Snapshot, targets: &[Target], picks: &[Pick], request: &str) -> Plan {
    let mut p = Plan { request: request.to_string(), ..Plan::default() };
    let mut channel = false;
    let mut tools: Vec<String> = Vec::new();
    let mut profile: Vec<String> = Vec::new();
    let mut flatpaks: Vec<String> = Vec::new();
    let mut flatpak_all = false;
    let mut firmware: Vec<String> = Vec::new();
    // mise's and uv's: every one with news, or the ones named.
    let mut mise = (false, Vec::<String>::new());
    let mut uv = (false, Vec::<String>::new());
    let categories: Vec<Category> =
        picks.iter().filter_map(|pk| if let Pick::Category(c) = pk { Some(*c) } else { None }).collect();
    let everything = categories.len() == Category::ALL.len();

    for c in &categories {
        match c {
            Category::System => {
                // Asked for by name but not checked: let the System Manager find out.
                if s.channel.has_update() || (!everything && s.channel.head.is_none() && s.channel.url.is_some()) {
                    channel = true;
                } else if let Some(e) = &s.channel.error {
                    p.notes.push(format!("The system channel couldn't be checked: {e}"));
                }
            }
            Category::Apps => {
                if s.channel.has_update() {
                    channel = true;
                    if !everything {
                        p.notes
                            .push("Nyx apps and apps from nixpkgs come with the system channel, so this updates it too.".into());
                    }
                }
                profile.extend(s.profile.iter().filter(|e| !e.tool && e.may_update()).map(|e| e.name.clone()));
                let updatable = s.flatpaks.iter().any(|f| f.update.is_some()) || (!s.checked && !s.flatpaks.is_empty());
                if updatable {
                    flatpak_all = true;
                }
            }
            Category::Tools => {
                for t in targets.iter().filter(|t| t.provider == Provider::ToolPin && t.has_update()) {
                    if t.needs_channel {
                        p.notes.push(format!(
                            "{} {} is built differently now, so it comes with the system update (nyx update system).",
                            t.name,
                            t.available.as_deref().unwrap_or_default()
                        ));
                        p.suggest_system = !everything;
                    } else {
                        tools.push(t.id.clone());
                    }
                }
                profile.extend(s.profile.iter().filter(|e| e.tool && e.may_update()).map(|e| e.name.clone()));
                let news = |tools: &[UserTool]| tools.iter().any(|t| t.update.is_some()) || (!s.checked && !tools.is_empty());
                mise.0 |= news(&s.mise);
                uv.0 |= news(&s.uv_tools);
                p.notes.extend(doubles(targets));
            }
            Category::Firmware => match &s.firmware {
                Firmware::Devices { devices } => {
                    let with: Vec<&Device> = devices.iter().filter(|d| d.update.is_some()).collect();
                    if with.is_empty() {
                        if !everything {
                            p.notes.push(if devices.is_empty() {
                                "No device here gets firmware updates through fwupd.".into()
                            } else {
                                "The firmware is up to date.".into()
                            });
                        }
                    } else {
                        firmware.extend(with.iter().map(|d| d.id.clone()));
                        p.firmware_implied = everything;
                    }
                }
                Firmware::Unavailable { reason } => p.notes.push(format!("Firmware: {reason}")),
                Firmware::NotChecked => {}
            },
        }
    }

    for pk in picks {
        let Pick::Target(i) = pk else { continue };
        let t = &targets[*i];
        match t.provider {
            Provider::Channel if t.category == Category::System => {
                if s.channel.has_update() || s.channel.head.is_none() {
                    channel = true;
                } else {
                    p.notes.push(format!("{} is up to date.", t.name));
                }
            }
            Provider::Channel => {
                if s.channel.has_update() {
                    channel = true;
                    p.notes.push(format!("{} comes with the system channel, so this updates the system.", t.name));
                } else {
                    p.notes.push(format!("{} is as new as the system channel has it.", t.name));
                }
            }
            Provider::ToolPin => match (&t.available, t.needs_channel) {
                (Some(v), true) => {
                    p.notes.push(format!("{} {v} is built differently now, so it comes with the system update.", t.name));
                    p.suggest_system = true;
                }
                (Some(_), false) => tools.push(t.id.clone()),
                (None, _) if s.head_tools.is_none() => {
                    p.notes.push(format!("{}: the channel couldn't be checked, so its newest version isn't known.", t.name))
                }
                (None, _) => p.notes.push(format!("{} is up to date ({}).", t.name, t.installed.as_deref().unwrap_or("?"))),
            },
            Provider::Profile => match s.profile.iter().find(|e| e.name == t.key) {
                Some(e) if !e.may_update() => {
                    p.notes.push(format!("{} is up to date ({}).", t.name, t.installed.as_deref().unwrap_or("?")))
                }
                _ => profile.push(t.key.clone()),
            },
            Provider::Flatpak => {
                if t.has_update() || !s.checked {
                    flatpaks.push(t.key.clone());
                } else {
                    p.notes.push(format!("{} is up to date.", t.name));
                }
            }
            Provider::Firmware => firmware.push(t.key.clone()),
            Provider::Mise | Provider::UvTool => {
                let list = if t.provider == Provider::Mise { &mut mise.1 } else { &mut uv.1 };
                if t.has_update() || !s.checked {
                    list.push(t.key.clone());
                } else {
                    p.notes.push(format!("{} is up to date ({}).", t.name, t.installed.as_deref().unwrap_or("?")));
                }
            }
        }
    }

    // The channel brings every tool's pin.
    if channel {
        p.steps.push(Step::System { channel: true, tools: Vec::new() });
        p.items.push(Item {
            name: targets[0].name.clone(),
            from: targets[0].installed.clone(),
            to: targets[0].available.clone(),
            how: "the system channel: NixOS, Nyx apps, apps from nixpkgs".into(),
        });
        for t in targets.iter().filter(|t| t.provider == Provider::ToolPin && t.has_update()) {
            p.items.push(Item {
                name: t.name.clone(),
                from: t.installed.clone(),
                to: t.available.clone(),
                how: "with the system channel".into(),
            });
        }
    } else if !tools.is_empty() {
        tools.sort();
        tools.dedup();
        for id in &tools {
            if let Some(t) = targets.iter().find(|t| &t.id == id) {
                p.items.push(Item {
                    name: t.name.clone(),
                    from: t.installed.clone(),
                    to: t.available.clone(),
                    how: how(t).into(),
                });
            }
        }
        p.steps.push(Step::System { channel: false, tools });
    }
    if !profile.is_empty() {
        profile.sort();
        profile.dedup();
        for e in s.profile.iter().filter(|e| profile.contains(&e.name)) {
            let how = if e.known { "your Nix profile" } else { "your Nix profile (only what has news changes)" };
            let name = e.title.clone().unwrap_or_else(|| e.name.clone());
            p.items.push(Item { name, from: e.version.clone(), to: e.update.clone(), how: how.into() });
        }
        p.steps.push(Step::Profile { names: profile });
    }
    if flatpak_all || !flatpaks.is_empty() {
        flatpaks.sort();
        flatpaks.dedup();
        let apps = if flatpak_all { Vec::new() } else { flatpaks };
        for f in s.flatpaks.iter().filter(|f| (apps.is_empty() && (f.update.is_some() || !s.checked)) || apps.contains(&f.id)) {
            p.items.push(Item { name: f.name.clone(), from: f.version.clone(), to: f.update.clone(), how: "a Flatpak".into() });
        }
        p.steps.push(Step::Flatpak { apps });
    }
    for (provider, (all, mut names), tools) in [(Provider::Mise, mise, &s.mise), (Provider::UvTool, uv, &s.uv_tools)] {
        if !all && names.is_empty() {
            continue;
        }
        names.sort();
        names.dedup();
        let names = if all { Vec::new() } else { names };
        let how = if provider == Provider::Mise { "your mise tools" } else { "your uv tools" };
        for t in tools.iter().filter(|t| (names.is_empty() && (t.update.is_some() || !s.checked)) || names.contains(&t.name)) {
            p.items.push(Item { name: t.name.clone(), from: t.version.clone(), to: t.update.clone(), how: how.into() });
        }
        p.steps.push(if provider == Provider::Mise { Step::Mise { tools: names } } else { Step::UvTools { tools: names } });
    }
    if !firmware.is_empty() {
        firmware.sort();
        firmware.dedup();
        if let Firmware::Devices { devices } = &s.firmware {
            for d in devices.iter().filter(|d| firmware.contains(&d.id)) {
                p.items.push(Item {
                    name: d.name.clone(),
                    from: Some(d.version.clone()),
                    to: d.update.clone(),
                    how: "firmware (can't be undone)".into(),
                });
                p.may_restart |= d.needs_restart;
            }
        }
        p.steps.push(Step::Firmware { devices: firmware });
    }
    p.admin = p.steps.iter().any(|s| matches!(s, Step::System { .. }));
    p.may_restart |= p.steps.iter().any(|s| matches!(s, Step::System { channel: true, .. }));
    p
}

/// What an unattended run may update in `categories`: the person's own software (their
/// profile, Flatpaks, mise and uv tools) that has news, never the system or firmware.
pub fn unattended(s: &Snapshot, targets: &[Target], categories: &[Category]) -> Vec<Pick> {
    targets
        .iter()
        .enumerate()
        .filter(|(_, t)| categories.contains(&t.category))
        .filter(|(_, t)| match t.provider {
            Provider::Profile => s.profile.iter().any(|e| e.name == t.key && e.may_update()),
            Provider::Flatpak | Provider::Mise | Provider::UvTool => t.has_update(),
            Provider::Channel | Provider::ToolPin | Provider::Firmware => false,
        })
        .map(|(i, _)| Pick::Target(i))
        .collect()
}

/// What's available, for `nyx update status` and the daily check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub available: Vec<Item>,
    pub notes: Vec<String>,
}

pub fn status(s: &Snapshot, targets: &[Target]) -> Status {
    let mut st = Status::default();
    for t in targets.iter().filter(|t| t.has_update()) {
        let how = if t.needs_channel { "with the system update" } else { how(t) };
        st.available.push(Item { name: t.name.clone(), from: t.installed.clone(), to: t.available.clone(), how: how.into() });
    }
    if let Some(e) = &s.channel.error {
        st.notes.push(format!("The system channel couldn't be checked: {e}"));
    }
    if let Firmware::Unavailable { reason } = &s.firmware {
        st.notes.push(format!("Firmware: {reason}"));
    }
    st.notes.extend(doubles(targets));
    st
}

/// How updates have gone, from the history: `nyx update report`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub entries: usize,
    /// The oldest entry looked at.
    pub since: Option<i64>,
    pub system_updated: usize,
    pub system_failed: usize,
    pub system_current: usize,
    pub rollbacks: usize,
    pub rollbacks_failed: usize,
    pub user_updated: usize,
    pub user_failed: usize,
    pub restarts: usize,
    /// The typical (median) seconds of each stage: `check`, `pins`, `download`, `build`,
    /// `switch`, `total` (of system updates that worked).
    pub median: BTreeMap<String, u64>,
    /// The longest system update: its request, when, and seconds.
    pub longest: Option<(String, i64, u64)>,
}

impl Report {
    /// Failed system updates out of those that ran, in percent.
    pub fn failure_rate(&self) -> Option<u64> {
        let ran = self.system_updated + self.system_failed;
        (ran > 0).then(|| (self.system_failed * 100 + ran / 2) as u64 / ran as u64)
    }
}

pub fn report(entries: &[nyx_system_proto::HistoryEntry]) -> Report {
    let mut r = Report { entries: entries.len(), since: entries.iter().map(|e| e.time).min(), ..Report::default() };
    let mut stages: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for e in entries {
        match (e.scope.as_str(), e.result.as_str(), e.request.as_str()) {
            ("system", "failed", "rollback") => r.rollbacks_failed += 1,
            ("system", _, "rollback") => r.rollbacks += 1,
            ("system", "failed", _) => r.system_failed += 1,
            ("system", "current", _) => r.system_current += 1,
            ("system", _, _) => r.system_updated += 1,
            (_, "failed", _) => r.user_failed += 1,
            _ => r.user_updated += 1,
        }
        r.restarts += usize::from(e.reboot);
        let system_update = e.scope == "system" && e.result == "updated";
        for (stage, secs) in &e.seconds {
            if stage == "check" || system_update {
                stages.entry(stage.clone()).or_default().push(*secs);
            }
        }
        if let (true, Some(total)) = (system_update, e.seconds.get("total"))
            && r.longest.as_ref().is_none_or(|l| *total > l.2)
        {
            r.longest = Some((e.request.clone(), e.time, *total));
        }
    }
    r.median = stages
        .into_iter()
        .map(|(stage, mut v)| {
            v.sort_unstable();
            (stage, v[v.len() / 2])
        })
        .collect();
    r
}

/// `4m 12s`.
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests;

//! The Nyx System Manager's contract: the system settings it keeps in the overrides
//! layer (`/etc/nyxos/overrides/system.json`, read by the NixOS configuration), the
//! typed changes clients may ask for, and its D-Bus interface.
//!
//! Every change is validated here, by clients before they ask and by the manager again
//! before it writes anything. Each kind of change has its own polkit action.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod hardware;

pub const BUS_NAME: &str = "io.github.nyxos_project.System";
pub const OBJECT_PATH: &str = "/io/github/nyxos_project/System";
pub const INTERFACE: &str = "io.github.nyxos_project.System1";

/// Polkit actions, one per kind of operation.
pub mod action {
    pub const SETTINGS: &str = "io.github.nyxos_project.system.settings";
    pub const PACKAGES: &str = "io.github.nyxos_project.system.packages";
    pub const SERVICES: &str = "io.github.nyxos_project.system.services";
    pub const DEFAULTS: &str = "io.github.nyxos_project.system.defaults";
    pub const UPDATE: &str = "io.github.nyxos_project.system.update";
    pub const ROLLBACK: &str = "io.github.nyxos_project.system.rollback";
    /// Joining or leaving a group in [`GROUPS`](super::GROUPS): asked for every time,
    /// since some (docker) amount to root.
    pub const GROUPS: &str = "io.github.nyxos_project.system.groups";
    /// One change of several kinds, asked for once: what of it still needs a password
    /// after each kind's own action is checked.
    pub const APPLY: &str = "io.github.nyxos_project.system.apply";
    /// Turning the firewall on or off, and opening ports in it.
    pub const FIREWALL: &str = "io.github.nyxos_project.system.firewall";
    /// Removing old generations and unused packages, and old system logs.
    pub const CLEAN: &str = "io.github.nyxos_project.system.clean";
    /// Adding and removing people's accounts, their names and administrator rights, and
    /// other people's passwords.
    pub const ACCOUNTS: &str = "io.github.nyxos_project.system.accounts";
    /// Changing your own password (asks for it).
    pub const PASSWORD: &str = "io.github.nyxos_project.system.password";
    /// Mounting drives and network shares at startup.
    pub const MOUNTS: &str = "io.github.nyxos_project.system.mounts";
    /// Reading the settings, history and generations: people at the computer, not
    /// other accounts (services, remote logins).
    pub const READ: &str = "io.github.nyxos_project.system.read";
}

/// The effective tool pins a system was built with, and whether it installs each
/// (written by the NyxOS configuration).
pub const TOOLS_FILE: &str = "/etc/nyxos/tools.json";

/// A tool as [`TOOLS_FILE`] and a channel's `pkgs/tools/sources.json` describe it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPin {
    #[serde(default)]
    pub name: String,
    pub version: String,
    pub recipe: u32,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Herdr's core, which can change without the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_version: Option<String>,
    /// Only in [`TOOLS_FILE`].
    #[serde(default)]
    pub installed: bool,
}

impl ToolPin {
    /// The version with what else can change in the same pin, as shown and compared.
    pub fn release(&self) -> String {
        match &self.core_version {
            Some(core) => format!("{} (core {core})", self.version),
            None => self.version.clone(),
        }
    }

    /// Whether `other` is a newer release of this tool.
    pub fn older_than(&self, other: &ToolPin) -> bool {
        let key = |p: &ToolPin| format!("{}-{}", p.version, p.core_version.as_deref().unwrap_or("0"));
        compare_versions(&key(self), &key(other)) == std::cmp::Ordering::Less
    }
}

/// Compare versions the way Nix's `builtins.compareVersions` does: numbers as numbers,
/// anything else as text, component by component.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            let boundary = cur.chars().last().is_some_and(|l| l.is_ascii_digit() != c.is_ascii_digit());
            if !c.is_ascii_alphanumeric() || boundary {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !c.is_ascii_alphanumeric() {
                    continue;
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
    // Nix's componentsLT: "pre" is older than anything, a missing part older than a number.
    fn lt(x: &str, y: &str) -> bool {
        let (m, n) = (x.parse::<u64>().ok(), y.parse::<u64>().ok());
        match (m, n) {
            (Some(m), Some(n)) => m < n,
            _ if x.is_empty() && n.is_some() => true,
            _ if x == "pre" && y != "pre" => true,
            _ if y == "pre" => false,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            _ => x < y,
        }
    }
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).map(String::as_str).unwrap_or(""), b.get(i).map(String::as_str).unwrap_or(""));
        if lt(x, y) {
            return std::cmp::Ordering::Less;
        }
        if lt(y, x) {
            return std::cmp::Ordering::Greater;
        }
    }
    std::cmp::Ordering::Equal
}

/// System services the manager may turn on or off, with what each is.
pub const SERVICES: &[(&str, &str)] = &[
    ("bluetooth", "Bluetooth"),
    ("printing", "Printing (CUPS)"),
    ("ssh", "Remote login (OpenSSH)"),
    ("flatpak", "Flatpak apps"),
    ("docker", "Docker containers"),
    ("tailscale", "Tailscale"),
    ("fwupd", "Firmware updates (fwupd)"),
    ("avahi", "Local network discovery (Avahi)"),
    ("podman", "Podman containers"),
    ("incus", "Incus containers and virtual machines"),
    ("libvirtd", "Virtual machines (libvirt)"),
    ("ollama", "Ollama, local AI models"),
];

/// Groups the manager may add users to, with what membership allows.
pub const GROUPS: &[(&str, &str)] = &[
    ("docker", "Run Docker containers (as good as root on this computer)"),
    ("libvirtd", "Manage system virtual machines"),
    ("incus-admin", "Manage Incus containers and virtual machines"),
    ("kvm", "Use hardware virtualization directly"),
    ("dialout", "Use serial ports"),
];

/// What logind does for each lid and button event (`[Login]` keys), and the actions
/// it takes.
pub const POWER_KEYS: &[&str] = &[
    "HandleLidSwitch",
    "HandleLidSwitchExternalPower",
    "HandleLidSwitchDocked",
    "HandlePowerKey",
    "HandlePowerKeyLongPress",
    "HandleSuspendKey",
    "HandleHibernateKey",
];
pub const POWER_ACTIONS: &[&str] =
    &["ignore", "poweroff", "reboot", "halt", "suspend", "hibernate", "hybrid-sleep", "suspend-then-hibernate", "lock"];

/// The firewall: on or off (absent leaves the distribution's), and the ports opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Firewall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tcp: BTreeSet<u16>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub udp: BTreeSet<u16>,
}

impl Firewall {
    fn is_empty(&self) -> bool {
        self == &Firewall::default()
    }
}

/// Lid, button and sleep behaviour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Power {
    /// [`POWER_KEYS`] and their [`POWER_ACTIONS`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub login: BTreeMap<String, String>,
    /// Seconds asleep before suspend-then-hibernate hibernates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hibernate_delay: Option<u32>,
}

impl Power {
    fn is_empty(&self) -> bool {
        self == &Power::default()
    }
}

/// An account NyxOS made for someone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Person {
    pub full_name: String,
    /// In the wheel group: may make system changes.
    #[serde(default)]
    pub admin: bool,
}

/// Where a share's login is kept (root only, never in the configuration).
pub const MOUNT_LOGINS: &str = "/etc/nyxos/secrets/mounts";

/// Filesystems a mount may have: drives' and network shares'.
pub const MOUNT_TYPES: &[&str] =
    &["ext4", "ext3", "ext2", "btrfs", "xfs", "f2fs", "vfat", "exfat", "ntfs3", "cifs", "nfs", "nfs4"];

/// A drive or network share mounted at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mount {
    /// A partition's UUID, or a share: `//server/share` (cifs), `server:/path` (nfs).
    pub what: String,
    pub dir: String,
    pub fs_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// A share login is kept in [`MOUNT_LOGINS`].
    #[serde(default)]
    pub login: bool,
}

impl Mount {
    pub fn validate(&self) -> Result<()> {
        ensure!(MOUNT_TYPES.contains(&self.fs_type.as_str()), "not a filesystem NyxOS mounts: {}", self.fs_type);
        let network = matches!(self.fs_type.as_str(), "cifs" | "nfs" | "nfs4");
        let host =
            |h: &str| !h.is_empty() && h.len() < 254 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_[]:".contains(&b));
        let text = |s: &str| !s.is_empty() && s.len() < 512 && !s.contains(['\n', ' ', '\t', ',', '\'', '"', '\\']);
        let ok = if !network {
            self.what.len() <= 64 && !self.what.is_empty() && self.what.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        } else if self.fs_type == "cifs" {
            self.what.strip_prefix("//").and_then(|r| r.split_once('/')).is_some_and(|(h, s)| host(h) && text(s))
        } else {
            self.what.split_once(":/").is_some_and(|(h, p)| host(h) && (p.is_empty() || text(p)))
        };
        ensure!(ok, "not a {}: {}", if network { "share address" } else { "partition UUID" }, self.what);
        ensure!(!self.login || self.fs_type == "cifs", "only Windows (SMB) shares take a login");
        let parts: Vec<&str> = self.dir.split('/').collect();
        ensure!(
            self.dir.len() < 256
                && self.dir.starts_with('/')
                && parts.iter().skip(1).all(|p| !p.is_empty() && *p != "." && *p != ".." && text(p))
                && (matches!(parts.get(1), Some(&"mnt") | Some(&"media")) && parts.len() > 2
                    || parts.get(1) == Some(&"home") && parts.len() > 3),
            "a mount folder is inside /mnt, /media or someone's home folder, not {}",
            self.dir
        );
        for o in &self.options {
            ensure!(
                !o.is_empty() && o.len() < 100 && o.bytes().all(|b| b.is_ascii_alphanumeric() || b"=_.-".contains(&b)),
                "not a mount option NyxOS sets: {o}"
            );
            ensure!(!o.starts_with("credentials") && !o.starts_with("x-systemd.requires"), "not a mount option NyxOS sets: {o}");
        }
        Ok(())
    }
}

/// The system layer's settings. Absent values leave the distribution's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SystemSettings {
    #[serde(default = "version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_layout: Option<String>,
    /// nixpkgs attributes installed for everyone.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub packages: BTreeSet<String>,
    /// Those of `packages` with an unfree license the administrator accepted.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unfree: BTreeSet<String>,
    /// Nyx apps (`nyx.<name>.enable`) turned on or off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nyx_apps: BTreeMap<String, bool>,
    /// [`GROUPS`] and the users added to each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, BTreeSet<String>>,
    /// [`SERVICES`] the administrator turned on or off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Themes::is_empty")]
    pub theme: Themes,
    #[serde(default, skip_serializing_if = "Firewall::is_empty")]
    pub firewall: Firewall,
    #[serde(default, skip_serializing_if = "Power::is_empty")]
    pub power: Power,
    /// Accounts NyxOS made, by login name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub people: BTreeMap<String, Person>,
    /// Full names of accounts declared elsewhere (the primary user's, from the host).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub full_names: BTreeMap<String, String>,
    /// Drives and shares mounted at startup, by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mounts: BTreeMap<String, Mount>,
    /// What the System Manager detected, and the driver choices made for it.
    #[serde(default, skip_serializing_if = "hardware::Hardware::is_empty")]
    pub hardware: hardware::Hardware,
    /// The system logo, in place of the Nyx mark (`/etc/nyx/logo.svg|png`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<Logo>,
}

/// A logo image. An SVG's `currentColor` parts take the theme's logo colour; a PNG is
/// shown as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logo {
    pub format: LogoFormat,
    /// The SVG's text, or the PNG in base64.
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogoFormat {
    Svg,
    Png,
}

pub const MAX_LOGO_SVG: usize = 512 * 1024;
pub const MAX_LOGO_PNG: usize = 2 * 1024 * 1024;

/// A logo the system may take: an SVG without scripts, foreign content, entities,
/// event handlers or references outside itself, or a PNG; each within its size.
pub fn valid_logo(l: &Logo) -> Result<()> {
    match l.format {
        LogoFormat::Png => {
            ensure!(l.data.len() <= MAX_LOGO_PNG.div_ceil(3) * 4, "the logo is over {} KB", MAX_LOGO_PNG / 1024);
            ensure!(l.data.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)), "the logo isn't base64");
            // The PNG signature, in base64.
            ensure!(l.data.starts_with("iVBORw0KGgo"), "the logo isn't a PNG");
        }
        LogoFormat::Svg => {
            ensure!(l.data.len() <= MAX_LOGO_SVG, "the logo is over {} KB", MAX_LOGO_SVG / 1024);
            let lower = l.data.to_ascii_lowercase();
            ensure!(lower.contains("<svg"), "the logo isn't an SVG image");
            for bad in
                ["<script", "<foreignobject", "<!entity", "<!doctype", "<iframe", "<embed", "<object", "javascript:", "@import"]
            {
                ensure!(!lower.contains(bad), "the logo contains {bad}");
            }
            let bytes = lower.as_bytes();
            for (i, _) in lower.match_indices("on") {
                let before = if i == 0 { b' ' } else { bytes[i - 1] };
                let end = lower[i + 2..].find(|c: char| !c.is_ascii_alphabetic()).map_or(lower.len(), |n| i + 2 + n);
                ensure!(
                    !(before.is_ascii_whitespace() && end > i + 2 && lower[end..].trim_start().starts_with('=')),
                    "the logo has an event handler"
                );
            }
            for (i, _) in lower.match_indices("href") {
                if let Some(v) = lower[i + 4..].trim_start().strip_prefix('=') {
                    let v = v.trim_start().trim_start_matches(['"', '\'']);
                    ensure!(v.starts_with('#') || v.starts_with("data:image/"), "the logo links outside itself");
                }
            }
            for (i, _) in lower.match_indices("url(") {
                let v = lower[i + 4..].trim_start().trim_start_matches(['"', '\'']);
                ensure!(v.starts_with('#'), "the logo refers to something outside itself");
            }
        }
    }
    Ok(())
}

fn version() -> u32 {
    1
}

impl Default for SystemSettings {
    fn default() -> Self {
        SystemSettings {
            version: version(),
            hostname: None,
            time_zone: None,
            locale: None,
            keyboard_layout: None,
            packages: BTreeSet::new(),
            unfree: BTreeSet::new(),
            nyx_apps: BTreeMap::new(),
            groups: BTreeMap::new(),
            services: BTreeMap::new(),
            theme: Themes::default(),
            firewall: Firewall::default(),
            power: Power::default(),
            people: BTreeMap::new(),
            full_names: BTreeMap::new(),
            mounts: BTreeMap::new(),
            hardware: hardware::Hardware::default(),
            logo: None,
        }
    }
}

/// The system's theme and the boot and login screens' own, when they differ.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Themes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

impl Themes {
    fn is_empty(&self) -> bool {
        self == &Themes::default()
    }
}

/// One requested change. `None` hands a setting back to the distribution's default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Change {
    Hostname {
        value: Option<String>,
    },
    TimeZone {
        value: Option<String>,
    },
    Locale {
        value: Option<String>,
    },
    KeyboardLayout {
        value: Option<String>,
    },
    SystemTheme {
        value: Option<String>,
    },
    BootTheme {
        value: Option<String>,
    },
    LoginTheme {
        value: Option<String>,
    },
    /// The system logo; `None` is the Nyx mark.
    Logo {
        value: Option<Logo>,
    },
    /// `unfree` names those of `packages` whose license the administrator accepted.
    AddPackages {
        packages: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unfree: Vec<String>,
    },
    RemovePackages {
        packages: Vec<String>,
    },
    Service {
        name: String,
        enabled: Option<bool>,
    },
    NyxApp {
        name: String,
        enabled: Option<bool>,
    },
    GroupMember {
        group: String,
        user: String,
        member: bool,
    },
    Firewall {
        enabled: Option<bool>,
    },
    /// `protocol` is `tcp` or `udp`.
    FirewallPort {
        port: u16,
        protocol: String,
        open: bool,
    },
    /// A [`POWER_KEYS`] key and one of [`POWER_ACTIONS`] (`None` gives it back).
    Power {
        key: String,
        action: Option<String>,
    },
    HibernateDelay {
        seconds: Option<u32>,
    },
    AddPerson {
        name: String,
        full_name: String,
        admin: bool,
    },
    /// An account NyxOS made; its home folder stays.
    RemovePerson {
        name: String,
    },
    PersonAdmin {
        name: String,
        admin: bool,
    },
    FullName {
        user: String,
        full_name: String,
    },
    AddMount {
        id: String,
        mount: Mount,
    },
    RemoveMount {
        id: String,
    },
    /// What the computer has. The System Manager detects it itself; what a client sends is
    /// replaced.
    Hardware {
        #[serde(default)]
        hardware: hardware::Hardware,
    },
    /// NVIDIA's own driver (unfree) instead of nouveau.
    NvidiaDriver {
        proprietary: bool,
    },
}

impl Change {
    /// The polkit action that authorizes it.
    pub fn action(&self) -> &'static str {
        match self {
            Change::AddPackages { .. } | Change::RemovePackages { .. } | Change::NyxApp { .. } => action::PACKAGES,
            Change::Service { .. } => action::SERVICES,
            Change::GroupMember { .. } => action::GROUPS,
            Change::Firewall { .. } | Change::FirewallPort { .. } => action::FIREWALL,
            Change::AddPerson { .. } | Change::RemovePerson { .. } | Change::PersonAdmin { .. } | Change::FullName { .. } => {
                action::ACCOUNTS
            }
            Change::AddMount { .. } | Change::RemoveMount { .. } => action::MOUNTS,
            _ => action::SETTINGS,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let opt = |v: &Option<String>, check: fn(&str) -> bool, what: &str| -> Result<()> {
            if let Some(v) = v {
                ensure!(check(v), "not a valid {what}: {v}");
            }
            Ok(())
        };
        match self {
            Change::Hostname { value } => opt(value, valid_hostname, "host name"),
            Change::TimeZone { value } => opt(value, valid_time_zone, "time zone"),
            Change::Locale { value } => opt(value, valid_locale, "locale"),
            Change::KeyboardLayout { value } => opt(value, valid_keyboard_layout, "keyboard layout"),
            Change::SystemTheme { value } | Change::BootTheme { value } | Change::LoginTheme { value } => {
                opt(value, valid_theme, "theme")
            }
            Change::Logo { value } => value.as_ref().map_or(Ok(()), valid_logo),
            Change::AddPackages { packages, unfree } => {
                valid_packages(packages)?;
                for u in unfree {
                    ensure!(packages.contains(u), "{u} is marked unfree but isn't one of the packages");
                }
                Ok(())
            }
            Change::RemovePackages { packages } => valid_packages(packages),
            Change::Service { name, .. } => {
                ensure!(SERVICES.iter().any(|(n, _)| n == name), "not a service NyxOS manages here: {name}");
                Ok(())
            }
            Change::NyxApp { name, .. } => {
                ensure!(valid_nyx_app(name), "not a valid Nyx app name: {name}");
                Ok(())
            }
            Change::GroupMember { group, user, .. } => {
                ensure!(GROUPS.iter().any(|(g, _)| g == group), "not a group NyxOS manages here: {group}");
                ensure!(valid_user(user), "not a valid user name: {user}");
                Ok(())
            }
            Change::Firewall { .. } => Ok(()),
            Change::FirewallPort { port, protocol, .. } => {
                ensure!(*port > 0, "port 0 isn't a port");
                ensure!(matches!(protocol.as_str(), "tcp" | "udp"), "the protocol is tcp or udp, not {protocol}");
                Ok(())
            }
            Change::Power { key, action } => {
                ensure!(POWER_KEYS.contains(&key.as_str()), "not a lid or button setting: {key}");
                if let Some(a) = action {
                    ensure!(POWER_ACTIONS.contains(&a.as_str()), "not something logind does: {a}");
                }
                Ok(())
            }
            Change::HibernateDelay { seconds } => {
                ensure!(seconds.is_none_or(|s| (60..=7 * 24 * 3600).contains(&s)), "between a minute and a week");
                Ok(())
            }
            Change::AddPerson { name, full_name, .. } => {
                ensure!(valid_user(name) && !reserved_user(name), "not a login name a person can have: {name}");
                ensure!(valid_full_name(full_name), "a name is up to 100 characters, without : or line breaks");
                Ok(())
            }
            Change::RemovePerson { name } | Change::PersonAdmin { name, .. } => {
                ensure!(valid_user(name), "not a valid user name: {name}");
                Ok(())
            }
            Change::FullName { user, full_name } => {
                ensure!(valid_user(user), "not a valid user name: {user}");
                ensure!(valid_full_name(full_name), "a name is up to 100 characters, without : or line breaks");
                Ok(())
            }
            Change::AddMount { id, mount } => {
                ensure!(valid_mount_id(id), "not a mount id: {id}");
                mount.validate()
            }
            Change::RemoveMount { id } => {
                ensure!(valid_mount_id(id), "not a mount id: {id}");
                Ok(())
            }
            Change::Hardware { hardware } => {
                let names =
                    |s: &str| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c));
                ensure!(
                    hardware.gpus.iter().all(|g| names(&g.vendor) && names(&g.bus))
                        && hardware.cpu.as_deref().is_none_or(names)
                        && hardware.virtual_machine.as_deref().is_none_or(names),
                    "the hardware description has something unexpected in it"
                );
                Ok(())
            }
            Change::NvidiaDriver { .. } => Ok(()),
        }
    }
}

impl SystemSettings {
    /// The settings with `changes` applied, after validating each.
    pub fn with(&self, changes: &[Change]) -> Result<SystemSettings> {
        let mut s = self.clone();
        for c in changes {
            c.validate()?;
            match c {
                Change::Hostname { value } => s.hostname = value.clone(),
                Change::TimeZone { value } => s.time_zone = value.clone(),
                Change::Locale { value } => s.locale = value.clone(),
                Change::KeyboardLayout { value } => s.keyboard_layout = value.clone(),
                Change::SystemTheme { value } => s.theme.system = value.clone(),
                Change::BootTheme { value } => s.theme.boot = value.clone(),
                Change::LoginTheme { value } => s.theme.login = value.clone(),
                Change::Logo { value } => s.logo = value.clone(),
                Change::AddPackages { packages, unfree } => {
                    s.packages.extend(packages.iter().cloned());
                    s.unfree.extend(unfree.iter().cloned());
                }
                Change::RemovePackages { packages } => {
                    for p in packages {
                        s.packages.remove(p);
                        s.unfree.remove(p);
                    }
                }
                Change::NyxApp { name, enabled: Some(on) } => {
                    s.nyx_apps.insert(name.clone(), *on);
                }
                Change::NyxApp { name, enabled: None } => {
                    s.nyx_apps.remove(name);
                }
                Change::Firewall { enabled } => s.firewall.enabled = *enabled,
                Change::FirewallPort { port, protocol, open } => {
                    let ports = if protocol == "udp" { &mut s.firewall.udp } else { &mut s.firewall.tcp };
                    if *open {
                        ports.insert(*port);
                    } else {
                        ports.remove(port);
                    }
                }
                Change::Power { key, action: Some(a) } => {
                    s.power.login.insert(key.clone(), a.clone());
                }
                Change::Power { key, action: None } => {
                    s.power.login.remove(key);
                }
                Change::HibernateDelay { seconds } => s.power.hibernate_delay = *seconds,
                Change::AddPerson { name, full_name, admin } => {
                    ensure!(!s.people.contains_key(name), "{name} already has an account");
                    s.people.insert(name.clone(), Person { full_name: full_name.clone(), admin: *admin });
                }
                Change::RemovePerson { name } => {
                    ensure!(
                        s.people.remove(name).is_some(),
                        "{name}'s account wasn't made by NyxOS, so it can't be removed here"
                    );
                    s.full_names.remove(name);
                }
                Change::PersonAdmin { name, admin } => match s.people.get_mut(name) {
                    Some(p) => p.admin = *admin,
                    None => bail!("{name}'s account wasn't made by NyxOS; its rights are set in this computer's configuration"),
                },
                Change::AddMount { id, mount } => {
                    ensure!(!s.mounts.contains_key(id), "there's already a mount called {id}");
                    ensure!(!s.mounts.values().any(|m| m.dir == mount.dir), "something already mounts at {}", mount.dir);
                    s.mounts.insert(id.clone(), mount.clone());
                }
                Change::RemoveMount { id } => {
                    ensure!(s.mounts.remove(id).is_some(), "there's no mount called {id}");
                }
                Change::FullName { user, full_name } => match s.people.get_mut(user) {
                    Some(p) => p.full_name = full_name.clone(),
                    // An empty name gives it back to the configuration.
                    None if full_name.is_empty() => {
                        s.full_names.remove(user);
                    }
                    None => {
                        s.full_names.insert(user.clone(), full_name.clone());
                    }
                },
                Change::GroupMember { group, user, member } => {
                    let users = s.groups.entry(group.clone()).or_default();
                    if *member {
                        users.insert(user.clone());
                    } else {
                        users.remove(user);
                    }
                    if users.is_empty() {
                        s.groups.remove(group);
                    }
                }
                Change::Service { name, enabled: Some(on) } => {
                    s.services.insert(name.clone(), *on);
                }
                Change::Service { name, enabled: None } => {
                    s.services.remove(name);
                }
                Change::Hardware { hardware } => {
                    let proprietary = s.hardware.nvidia_proprietary;
                    s.hardware = hardware::Hardware { nvidia_proprietary: proprietary, ..hardware.clone() };
                }
                Change::NvidiaDriver { proprietary } => {
                    ensure!(!*proprietary || s.hardware.has_gpu("nvidia"), "this computer has no NVIDIA graphics");
                    s.hardware.nvidia_proprietary = *proprietary;
                }
            }
        }
        Ok(s)
    }

    pub fn parse(text: &str) -> Result<SystemSettings> {
        let s: SystemSettings = serde_json::from_str(text)?;
        if s.version != 1 {
            bail!("system settings version {} isn't one this manager knows", s.version);
        }
        Ok(s)
    }
}

/// RFC 1123 labels, one of them: what NixOS's networking.hostName accepts.
pub fn valid_hostname(h: &str) -> bool {
    (1..=63).contains(&h.len())
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-')
}

/// `Region/City` names as tzdata spells them.
pub fn valid_time_zone(tz: &str) -> bool {
    (1..=64).contains(&tz.len())
        && !tz.starts_with('/')
        && !tz.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        && tz.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b))
}

/// `en_US.UTF-8`.
pub fn valid_locale(l: &str) -> bool {
    let Some((lang, rest)) = l.split_once('_') else { return false };
    let Some((region, charset)) = rest.split_once('.') else { return false };
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && region.len() == 2
        && region.bytes().all(|b| b.is_ascii_uppercase())
        && charset == "UTF-8"
}

/// An XKB layout name (`us`, `de`, `gb`), optionally with comma-separated others.
pub fn valid_keyboard_layout(k: &str) -> bool {
    (1..=64).contains(&k.len()) && k.split(',').all(|p| (1..=16).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_lowercase()))
}

pub fn valid_theme(t: &str) -> bool {
    (1..=64).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A nixpkgs attribute path (`gimp`, `kdePackages.kdenlive`).
pub fn valid_package(p: &str) -> bool {
    (1..=100).contains(&p.len())
        && p.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
                && !part.starts_with(|c: char| c == '-' || c.is_ascii_digit())
        })
}

fn valid_packages(packages: &[String]) -> Result<()> {
    ensure!(!packages.is_empty() && packages.len() <= 100, "give between 1 and 100 packages");
    for p in packages {
        ensure!(valid_package(p), "not a valid nixpkgs attribute: {p}");
    }
    Ok(())
}

/// A Nyx app as its NixOS option names it (`media-player` for `nyx.media-player`).
pub fn valid_nyx_app(n: &str) -> bool {
    (1..=40).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
}

/// A login name as useradd takes it.
pub fn valid_user(u: &str) -> bool {
    (1..=32).contains(&u.len())
        && u.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        && u.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
}

/// `nas-photos`.
pub fn valid_mount_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
}

/// Names that aren't people's accounts.
pub fn reserved_user(u: &str) -> bool {
    matches!(u, "root" | "nobody" | "daemon" | "bin" | "sys" | "sshd" | "messagebus" | "polkituser" | "greeter" | "nixos")
        || u.starts_with("nixbld")
        || u.starts_with("systemd-")
}

/// A full name as `/etc/passwd`'s comment field takes it.
pub fn valid_full_name(n: &str) -> bool {
    n.chars().count() <= 100 && !n.contains([':', '\n', '\r', ','])
}

/// What [`System::upgrade`] does, in one rebuild: update the channel (NixOS, nixpkgs,
/// Nyx apps and the channel's tool pins), and/or bring single tools to the pins at the
/// head of the channel. The manager fetches those pins itself; a client only names tools.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpgradeRequest {
    #[serde(default)]
    pub channel: bool,
    /// Tool pin names (`claude-code`), ignored when `channel` is set: the channel brings
    /// every tool's pin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// What the person asked for (`nyx update claude`), for the history.
    #[serde(default)]
    pub request: String,
}

impl UpgradeRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.channel || !self.tools.is_empty(), "nothing to update");
        ensure!(self.tools.len() <= 50, "too many tools");
        for t in &self.tools {
            ensure!(valid_tool(t), "not a tool name: {t}");
        }
        ensure!(self.request.len() <= 500 && !self.request.contains(['\n', '\0']), "the request is one line");
        Ok(())
    }
}

/// A tool pin's name (`claude-code`).
pub fn valid_tool(t: &str) -> bool {
    (1..=40).contains(&t.len())
        && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !t.starts_with('-')
}

/// One package whose version changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionChange {
    pub name: String,
    pub from: String,
    pub to: String,
}

/// What an upgrade did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeResult {
    pub summary: String,
    pub generation_before: Option<u32>,
    pub generation_after: Option<u32>,
    /// The kernel, initrd or systemd changed: a restart finishes the update.
    pub reboot: bool,
    /// Programs still running from files the update replaced.
    #[serde(default)]
    pub restart: Vec<String>,
    #[serde(default)]
    pub changes: Vec<VersionChange>,
    /// How long each stage took ([`HistoryEntry::seconds`]).
    #[serde(default)]
    pub seconds: BTreeMap<String, u64>,
}

/// One entry in the update history (`/var/lib/nyx-system/history.jsonl` for the
/// system's, `~/.local/state/nyx/update-history.jsonl` for a user's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Seconds since the epoch.
    pub time: i64,
    /// `system` or `user`.
    pub scope: String,
    pub request: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub changes: Vec<VersionChange>,
    #[serde(default)]
    pub generation_before: Option<u32>,
    #[serde(default)]
    pub generation_after: Option<u32>,
    /// `updated`, `current` or `failed`.
    pub result: String,
    #[serde(default)]
    pub reboot: bool,
    #[serde(default)]
    pub restart: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// How long each stage took, in seconds: `check` (finding what's new), `pins`,
    /// `download`, `build`, `switch`, and `total`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seconds: BTreeMap<String, u64>,
}

/// History files keep this many entries.
pub const HISTORY_KEEP: usize = 200;

/// Add `entry` to the history file at `path`, keeping the last [`HISTORY_KEEP`].
pub fn append_history(path: &std::path::Path, entry: &HistoryEntry) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> =
        std::fs::read_to_string(path).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
    lines.push(serde_json::to_string(entry)?);
    let skip = lines.len().saturating_sub(HISTORY_KEEP);
    let tmp = path.with_extension("jsonl.new");
    std::fs::write(&tmp, lines[skip..].join("\n") + "\n")?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// The last `limit` entries of the history file at `path`, newest first.
pub fn read_history(path: &std::path::Path, limit: usize) -> Vec<HistoryEntry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(limit).collect()
}

/// One system generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub number: u32,
    /// Seconds since the epoch.
    pub created: i64,
    pub version: String,
    pub current: bool,
}

#[cfg(feature = "client")]
#[zbus::proxy(
    interface = "io.github.nyxos_project.System1",
    default_service = "io.github.nyxos_project.System",
    default_path = "/io/github/nyxos_project/System"
)]
pub trait System {
    /// The system layer's settings (JSON [`SystemSettings`]).
    fn settings(&self) -> zbus::Result<String>;
    /// Apply JSON [`Change`]s: validated, written, built, then switched to; undone if
    /// the build or the switch fails. Returns a summary.
    fn apply(&self, changes: &str) -> zbus::Result<String>;
    /// Set a distribution-wide default for every user (`/etc/nyxos/overrides/DOMAIN.toml`);
    /// `value` is JSON. Takes effect without a rebuild.
    fn set_default(&self, domain: &str, key: &str, value: &str) -> zbus::Result<()>;
    fn reset_default(&self, domain: &str, key: &str) -> zbus::Result<()>;
    /// Update the system's inputs (NyxOS, nixpkgs, Nyx apps) and switch to the result.
    fn update(&self) -> zbus::Result<String>;
    /// Update the channel and/or single tools (JSON [`UpgradeRequest`]) in one rebuild;
    /// returns JSON [`UpgradeResult`]. Progress lines that start with `== ` name the stage.
    fn upgrade(&self, request: &str) -> zbus::Result<String>;
    /// The system's update history (JSON [`HistoryEntry`]s), newest first.
    fn history(&self, limit: u32) -> zbus::Result<String>;
    /// [`apply`](Self::apply), then [`update`](Self::update), with one authorization
    /// for both. Empty `changes` (`[]`) only updates.
    fn apply_and_update(&self, changes: &str) -> zbus::Result<String>;
    /// The system generations (JSON [`Generation`]s), newest first.
    fn generations(&self) -> zbus::Result<String>;
    /// Switch back to the generation before the current one.
    fn rollback(&self) -> zbus::Result<String>;
    /// Remove generations older than this many days (never the current one) and what
    /// nothing needs any more; refreshes the boot menu. Returns what was freed.
    fn collect_garbage(&self, older_than_days: u32) -> zbus::Result<String>;
    /// Shrink the system journal to this many megabytes.
    fn vacuum_journal(&self, megabytes: u32) -> zbus::Result<String>;
    /// Set a person's password (asks for your own when it's yours, an administrator's
    /// otherwise). Never stored in the configuration.
    fn set_password(&self, user: &str, password: &str) -> zbus::Result<()>;
    /// Mount a drive or share at startup (`mount` is JSON [`Mount`]), with a share's
    /// login (`username=…` lines) kept apart; empty for none.
    fn add_mount(&self, id: &str, mount: &str, login: &str) -> zbus::Result<String>;
    /// Add an account and set its password, with one authorization.
    fn add_person(&self, name: &str, full_name: &str, admin: bool, password: &str) -> zbus::Result<String>;
    /// Trust the Nyx apps in the caller's own Nix profile that NyxOS's cache built
    /// (`/etc/nyx/trusted-bin`); returns how many.
    fn trust_apps(&self) -> zbus::Result<u32>;
    /// A line of build or switch output.
    #[zbus(signal)]
    fn progress(&self, line: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logos_are_checked() {
        let svg = |data: &str| Logo { format: LogoFormat::Svg, data: data.into() };
        let ok = svg("<svg viewBox=\"0 0 1 1\"><circle fill=\"currentColor\" r=\"1\"/><rect fill=\"url(#g)\"/></svg>");
        let s = SystemSettings::default().with(&[Change::Logo { value: Some(ok.clone()) }]).unwrap();
        assert_eq!(s.logo, Some(ok));
        assert!(serde_json::to_string(&s).unwrap().contains("\"logo\":{\"format\":\"svg\""));
        assert_eq!(s.with(&[Change::Logo { value: None }]).unwrap().logo, None);
        for bad in [
            "<svg><script/></svg>",
            "<svg onload=\"x()\"/>",
            "<svg><image href=\"https://x/a.png\"/></svg>",
            "<svg fill=\"url(file:///x)\"/>",
            "<!DOCTYPE svg><svg/>",
            "plain text",
        ] {
            assert!(Change::Logo { value: Some(svg(bad)) }.validate().is_err(), "{bad}");
        }
        let png = |data: &str| Change::Logo { value: Some(Logo { format: LogoFormat::Png, data: data.into() }) };
        assert!(png("iVBORw0KGgoAAAANSUhEUg==").validate().is_ok());
        assert!(png("R0lGODlh").validate().is_err(), "a GIF isn't a PNG");
        assert!(png("iVBORw0KGgo$$$").validate().is_err());
    }

    #[test]
    fn changes_are_validated_and_applied() {
        let s = SystemSettings::default()
            .with(&[
                Change::Hostname { value: Some("nyx-laptop".into()) },
                Change::AddPackages {
                    packages: vec!["gimp".into(), "kdePackages.kdenlive".into(), "vscode".into()],
                    unfree: vec!["vscode".into()],
                },
                Change::NyxApp { name: "mail".into(), enabled: Some(true) },
                Change::GroupMember { group: "docker".into(), user: "ken".into(), member: true },
                Change::Service { name: "printing".into(), enabled: Some(true) },
                Change::BootTheme { value: Some("nyx-dark".into()) },
            ])
            .unwrap();
        assert_eq!(s.hostname.as_deref(), Some("nyx-laptop"));
        assert_eq!(s.packages.len(), 3);
        assert!(s.unfree.contains("vscode") && s.nyx_apps.get("mail") == Some(&true));
        assert!(s.groups["docker"].contains("ken"));
        let s = s
            .with(&[
                Change::Firewall { enabled: Some(true) },
                Change::FirewallPort { port: 8080, protocol: "tcp".into(), open: true },
                Change::Power { key: "HandleLidSwitch".into(), action: Some("suspend-then-hibernate".into()) },
                Change::HibernateDelay { seconds: Some(1800) },
            ])
            .unwrap();
        assert!(s.firewall.tcp.contains(&8080) && s.power.login["HandleLidSwitch"] == "suspend-then-hibernate");
        assert_eq!(SystemSettings::parse(&serde_json::to_string(&s).unwrap()).unwrap(), s);
        let s = s
            .with(&[
                Change::AddPerson { name: "ana".into(), full_name: "Ana Smith".into(), admin: false },
                Change::PersonAdmin { name: "ana".into(), admin: true },
                Change::FullName { user: "ken".into(), full_name: "Ken S.".into() },
            ])
            .unwrap();
        assert!(s.people["ana"].admin && s.full_names["ken"] == "Ken S.");
        assert!(s.with(&[Change::FullName { user: "ken".into(), full_name: String::new() }]).unwrap().full_names.is_empty());
        assert!(s.with(&[Change::RemovePerson { name: "ken".into() }]).is_err());
        assert!(s.with(&[Change::RemovePerson { name: "ana".into() }]).unwrap().people.is_empty());
        let nas = Mount {
            what: "//nas.local/photos".into(),
            dir: "/mnt/photos".into(),
            fs_type: "cifs".into(),
            options: vec!["nofail".into(), "x-systemd.automount".into()],
            login: true,
        };
        let s = s.with(&[Change::AddMount { id: "photos".into(), mount: nas.clone() }]).unwrap();
        assert!(s.with(&[Change::AddMount { id: "photos2".into(), mount: nas }]).is_err(), "the same folder twice");
        let disk = Mount {
            what: "0a1b-2c3d".into(),
            dir: "/home/ken/Data".into(),
            fs_type: "exfat".into(),
            options: vec![],
            login: false,
        };
        assert!(s.with(&[Change::AddMount { id: "data".into(), mount: disk }]).is_ok());
        assert_eq!(s.services.get("printing"), Some(&true));
        let back = s
            .with(&[
                Change::RemovePackages { packages: vec!["gimp".into(), "vscode".into()] },
                Change::Hostname { value: None },
                Change::GroupMember { group: "docker".into(), user: "ken".into(), member: false },
            ])
            .unwrap();
        assert!(back.hostname.is_none() && back.packages.len() == 1 && back.unfree.is_empty() && back.groups.is_empty());
        let json = serde_json::to_string(&back).unwrap();
        assert_eq!(SystemSettings::parse(&json).unwrap(), back);
    }

    #[test]
    fn bad_values_are_refused() {
        for bad in [
            Change::Hostname { value: Some("-bad".into()) },
            Change::TimeZone { value: Some("../../etc/shadow".into()) },
            Change::Locale { value: Some("en_US".into()) },
            Change::KeyboardLayout { value: Some("us;rm".into()) },
            Change::AddPackages { packages: vec!["gimp; rm -rf /".into()], unfree: vec![] },
            Change::AddPackages { packages: vec![".hidden".into()], unfree: vec![] },
            Change::AddPackages { packages: vec!["gimp".into()], unfree: vec!["vscode".into()] },
            Change::NyxApp { name: "../mail".into(), enabled: Some(true) },
            Change::GroupMember { group: "wheel".into(), user: "ken".into(), member: true },
            Change::GroupMember { group: "docker".into(), user: "Ken;".into(), member: true },
            Change::FirewallPort { port: 22, protocol: "sctp".into(), open: true },
            Change::Power { key: "KillUserProcesses".into(), action: Some("yes".into()) },
            Change::Power { key: "HandleLidSwitch".into(), action: Some("rm -rf".into()) },
            Change::HibernateDelay { seconds: Some(1) },
            Change::AddPerson { name: "root".into(), full_name: "x".into(), admin: true },
            Change::AddPerson { name: "ana".into(), full_name: "Ana:0:0".into(), admin: false },
            Change::AddMount {
                id: "x".into(),
                mount: Mount {
                    what: "//nas/share".into(),
                    dir: "/etc".into(),
                    fs_type: "cifs".into(),
                    options: vec![],
                    login: false,
                },
            },
            Change::AddMount {
                id: "x".into(),
                mount: Mount {
                    what: "//nas/share".into(),
                    dir: "/mnt/nas".into(),
                    fs_type: "cifs".into(),
                    options: vec!["credentials=/etc/shadow".into()],
                    login: false,
                },
            },
            Change::AddMount {
                id: "x".into(),
                mount: Mount {
                    what: "//nas/a b".into(),
                    dir: "/mnt/nas".into(),
                    fs_type: "cifs".into(),
                    options: vec![],
                    login: false,
                },
            },
            Change::AddMount {
                id: "x".into(),
                mount: Mount {
                    what: "ab12".into(),
                    dir: "/mnt/../etc".into(),
                    fs_type: "ext4".into(),
                    options: vec![],
                    login: false,
                },
            },
            Change::Service { name: "sshd-root".into(), enabled: Some(true) },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(valid_time_zone("America/Chicago") && valid_locale("en_US.UTF-8") && valid_keyboard_layout("us,de"));
        assert!(SystemSettings::parse(r#"{"version":1,"rootPassword":"x"}"#).is_err());
    }

    #[test]
    fn upgrade_requests_are_validated() {
        let ok = UpgradeRequest { channel: false, tools: vec!["claude-code".into()], request: "nyx update claude".into() };
        assert!(ok.validate().is_ok());
        assert!(UpgradeRequest::default().validate().is_err(), "nothing to do");
        for bad in ["../x", "Claude", "a b", "-x", ""] {
            let r = UpgradeRequest { tools: vec![bad.into()], ..Default::default() };
            assert!(r.validate().is_err(), "{bad}");
        }
        let r = UpgradeRequest { channel: true, request: "x\ny".into(), ..Default::default() };
        assert!(r.validate().is_err());
    }

    #[test]
    fn versions_compare_like_nix() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("2.1.152", "2.1.311"), Less);
        assert_eq!(compare_versions("0.100.0", "0.70.0"), Greater);
        assert_eq!(compare_versions("2026.05.16-0338208", "2026.11.05-86047df"), Less);
        assert_eq!(compare_versions("1.0", "1.0.1"), Less);
        assert_eq!(compare_versions("1.0pre1", "1.0"), Less);
        assert_eq!(compare_versions("1.5.1", "1.5.1"), Equal);
        let herdr = |v: &str, c: &str| ToolPin { version: v.into(), core_version: Some(c.into()), ..Default::default() };
        assert!(herdr("0.1.13", "0.1.18").older_than(&herdr("0.1.13", "0.1.19")), "a new core alone is newer");
        assert!(!herdr("0.1.13", "0.1.18").older_than(&herdr("0.1.13", "0.1.18")));
        assert_eq!(herdr("0.1.13", "0.1.18").release(), "0.1.13 (core 0.1.18)");
    }

    #[test]
    fn history_keeps_the_last_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h/history.jsonl");
        for i in 0..(HISTORY_KEEP + 5) {
            let e = HistoryEntry { time: i as i64, scope: "system".into(), result: "updated".into(), ..Default::default() };
            append_history(&path, &e).unwrap();
        }
        let all = read_history(&path, 1000);
        assert_eq!(all.len(), HISTORY_KEEP);
        assert_eq!(all[0].time, (HISTORY_KEEP + 4) as i64, "newest first");
        assert_eq!(read_history(&path, 3).len(), 3);
    }
}

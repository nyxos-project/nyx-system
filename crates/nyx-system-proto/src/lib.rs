//! The Nyx System Manager's contract: the system settings it keeps in the overrides
//! layer (`/etc/nyxos/overrides/system.json`, read by the NixOS configuration), the
//! typed changes clients may ask for, and its D-Bus interface.
//!
//! Every change is validated here, by clients before they ask and by the manager again
//! before it writes anything. Each kind of change has its own polkit action.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
];

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
    /// [`SERVICES`] the administrator turned on or off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Themes::is_empty")]
    pub theme: Themes,
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
            services: BTreeMap::new(),
            theme: Themes::default(),
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
    Hostname { value: Option<String> },
    TimeZone { value: Option<String> },
    Locale { value: Option<String> },
    KeyboardLayout { value: Option<String> },
    SystemTheme { value: Option<String> },
    BootTheme { value: Option<String> },
    LoginTheme { value: Option<String> },
    AddPackages { packages: Vec<String> },
    RemovePackages { packages: Vec<String> },
    Service { name: String, enabled: Option<bool> },
}

impl Change {
    /// The polkit action that authorizes it.
    pub fn action(&self) -> &'static str {
        match self {
            Change::AddPackages { .. } | Change::RemovePackages { .. } => action::PACKAGES,
            Change::Service { .. } => action::SERVICES,
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
            Change::AddPackages { packages } | Change::RemovePackages { packages } => {
                ensure!(!packages.is_empty() && packages.len() <= 100, "give between 1 and 100 packages");
                for p in packages {
                    ensure!(valid_package(p), "not a valid nixpkgs attribute: {p}");
                }
                Ok(())
            }
            Change::Service { name, .. } => {
                ensure!(SERVICES.iter().any(|(n, _)| n == name), "not a service NyxOS manages here: {name}");
                Ok(())
            }
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
                Change::AddPackages { packages } => s.packages.extend(packages.iter().cloned()),
                Change::RemovePackages { packages } => {
                    for p in packages {
                        s.packages.remove(p);
                    }
                }
                Change::Service { name, enabled: Some(on) } => {
                    s.services.insert(name.clone(), *on);
                }
                Change::Service { name, enabled: None } => {
                    s.services.remove(name);
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
    /// The system generations (JSON [`Generation`]s), newest first.
    fn generations(&self) -> zbus::Result<String>;
    /// Switch back to the generation before the current one.
    fn rollback(&self) -> zbus::Result<String>;
    /// A line of build or switch output.
    #[zbus(signal)]
    fn progress(&self, line: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_validated_and_applied() {
        let s = SystemSettings::default()
            .with(&[
                Change::Hostname { value: Some("nyx-laptop".into()) },
                Change::AddPackages { packages: vec!["gimp".into(), "kdePackages.kdenlive".into()] },
                Change::Service { name: "printing".into(), enabled: Some(true) },
                Change::BootTheme { value: Some("nyx-dark".into()) },
            ])
            .unwrap();
        assert_eq!(s.hostname.as_deref(), Some("nyx-laptop"));
        assert_eq!(s.packages.len(), 2);
        assert_eq!(s.services.get("printing"), Some(&true));
        let back = s.with(&[Change::RemovePackages { packages: vec!["gimp".into()] }, Change::Hostname { value: None }]).unwrap();
        assert!(back.hostname.is_none() && back.packages.len() == 1);
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
            Change::AddPackages { packages: vec!["gimp; rm -rf /".into()] },
            Change::AddPackages { packages: vec![".hidden".into()] },
            Change::Service { name: "sshd-root".into(), enabled: Some(true) },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(valid_time_zone("America/Chicago") && valid_locale("en_US.UTF-8") && valid_keyboard_layout("us,de"));
        assert!(SystemSettings::parse(r#"{"version":1,"rootPassword":"x"}"#).is_err());
    }
}

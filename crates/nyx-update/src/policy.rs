//! The update settings (the `updates` domain of the configuration layers) and the
//! network: checking waits for a connection and, unless allowed, for one that isn't
//! metered.

use crate::Category;
use nyx_config::Layers;
use serde::{Deserialize, Serialize};

/// What the daily run does with one kind of update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Not checked.
    Off,
    /// Checked (Settings and `nyx update status` show it), without a notification.
    Check,
    /// Checked, with a notification; installed when the person says.
    Notify,
    /// Installed by the daily run. Only the person's own software is: the system and
    /// firmware are never installed unattended, so for them it means notify.
    Auto,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Policy> {
        Some(match s {
            "off" => Policy::Off,
            "check" => Policy::Check,
            "notify" => Policy::Notify,
            "auto" => Policy::Auto,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Check => "check",
            Policy::Notify => "notify",
            Policy::Auto => "auto",
        }
    }
}

/// `nyx-system config set updates KEY VALUE`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// The daily check runs.
    pub check: bool,
    /// It notifies when something new is available.
    pub notify: bool,
    /// It checks (and installs) on metered connections too.
    pub check_on_metered: bool,
    /// Checks include firmware.
    pub firmware: bool,
    /// What the daily run does with each kind of update (`updates.system`, `.apps`,
    /// `.tools`, `.firmware`).
    pub system: Policy,
    pub apps: Policy,
    pub tools: Policy,
    pub firmware_policy: Policy,
    /// Automatic installs wait for this time of day (`updates.install-at = "03:00"`).
    pub install_at: Option<(u8, u8)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            check: true,
            notify: true,
            check_on_metered: false,
            firmware: true,
            system: Policy::Notify,
            apps: Policy::Auto,
            tools: Policy::Auto,
            firmware_policy: Policy::Notify,
            install_at: None,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        Settings::from_layers(&Layers::standard())
    }

    pub fn from_layers(layers: &Layers) -> Settings {
        let d = Settings::default();
        let flag = |key: &str, default: bool| match layers.get("updates", key) {
            Ok(Some((toml::Value::Boolean(b), _))) => b,
            _ => default,
        };
        let policy = |key: &str, default: Policy| match layers.get("updates", key) {
            Ok(Some((toml::Value::String(s), _))) => Policy::parse(&s).unwrap_or(default),
            _ => default,
        };
        let install_at = match layers.get("updates", "install-at") {
            Ok(Some((toml::Value::String(s), _))) => parse_time(&s),
            _ => None,
        };
        Settings {
            check: flag("check", d.check),
            notify: flag("notify", d.notify),
            check_on_metered: flag("check-on-metered", d.check_on_metered),
            firmware: flag("firmware", d.firmware),
            system: policy("system", d.system),
            apps: policy("apps", d.apps),
            tools: policy("tools", d.tools),
            firmware_policy: policy("firmware-policy", d.firmware_policy),
            install_at,
        }
    }

    /// What the daily run does with `c`: off when checks are off; never installing the
    /// system or firmware by itself.
    pub fn policy(&self, c: Category) -> Policy {
        if !self.check {
            return Policy::Off;
        }
        let p = match c {
            Category::System => self.system,
            Category::Apps => self.apps,
            Category::Tools => self.tools,
            Category::Firmware if !self.firmware => Policy::Off,
            Category::Firmware => self.firmware_policy,
        };
        match (c, p) {
            (Category::System | Category::Firmware, Policy::Auto) => Policy::Notify,
            (_, p) => p,
        }
    }

    /// The categories with `policy`.
    pub fn with(&self, policy: Policy) -> Vec<Category> {
        Category::ALL.into_iter().filter(|c| self.policy(*c) == policy).collect()
    }
}

/// `HH:MM`.
pub fn parse_time(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u8, u8) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// Whether `now` (hours, minutes) is within the hour from `at`.
pub fn within(at: (u8, u8), now: (u8, u8)) -> bool {
    let (a, n) = (u32::from(at.0) * 60 + u32::from(at.1), u32::from(now.0) * 60 + u32::from(now.1));
    (n + 1440 - a) % 1440 < 60
}

/// What NetworkManager says about the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Online,
    Metered,
    Offline,
    /// NetworkManager couldn't be asked: go ahead and let the downloads say.
    Unknown,
}

/// NetworkManager's `State` and `Metered`.
pub async fn network() -> Network {
    async fn ask() -> zbus::Result<(u32, u32)> {
        let conn = zbus::Connection::system().await?;
        let nm = zbus::Proxy::new(
            &conn,
            "org.freedesktop.NetworkManager",
            "/org/freedesktop/NetworkManager",
            "org.freedesktop.NetworkManager",
        )
        .await?;
        let state: u32 = nm.get_property("State").await?;
        let metered: u32 = nm.get_property("Metered").await?;
        Ok((state, metered))
    }
    match ask().await {
        Ok((state, metered)) => from_nm(state, metered),
        Err(_) => Network::Unknown,
    }
}

/// NM_STATE (70 is connected to the internet) and NMMetered (1 yes, 3 guessed yes).
fn from_nm(state: u32, metered: u32) -> Network {
    if state < 60 {
        Network::Offline
    } else if matches!(metered, 1 | 3) {
        Network::Metered
    } else {
        Network::Online
    }
}

/// Why a check shouldn't run now, if it shouldn't.
pub fn deferred(settings: &Settings, net: Network) -> Option<&'static str> {
    match net {
        Network::Offline => Some("there's no network connection"),
        Network::Metered if !settings.check_on_metered => {
            Some("the connection is metered (nyx-system config set updates check-on-metered true allows it)")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_come_from_the_layers() {
        let dir = tempfile::tempdir().unwrap();
        let layers = Layers { defaults: dir.path().join("d"), system: dir.path().join("s"), user: dir.path().join("u") };
        assert_eq!(Settings::from_layers(&layers), Settings::default());
        layers.set(nyx_config::Layer::User, "updates", "check-on-metered", toml::Value::Boolean(true)).unwrap();
        layers.set(nyx_config::Layer::User, "updates", "firmware", toml::Value::Boolean(false)).unwrap();
        let s = Settings::from_layers(&layers);
        assert!(s.check_on_metered && !s.firmware && s.check);
    }

    #[test]
    fn policies_per_kind() {
        let dir = tempfile::tempdir().unwrap();
        let layers = Layers { defaults: dir.path().join("d"), system: dir.path().join("s"), user: dir.path().join("u") };
        let s = Settings::from_layers(&layers);
        assert_eq!(
            (s.policy(Category::System), s.policy(Category::Apps), s.policy(Category::Tools), s.policy(Category::Firmware)),
            (Policy::Notify, Policy::Auto, Policy::Auto, Policy::Notify)
        );
        let set = |k: &str, v: &str| layers.set(nyx_config::Layer::User, "updates", k, toml::Value::String(v.into())).unwrap();
        set("system", "auto");
        set("firmware-policy", "auto");
        set("apps", "check");
        set("install-at", "03:30");
        let s = Settings::from_layers(&layers);
        assert_eq!(s.policy(Category::System), Policy::Notify, "the system is never installed unattended");
        assert_eq!(s.policy(Category::Firmware), Policy::Notify, "nor firmware");
        assert_eq!(s.policy(Category::Apps), Policy::Check);
        assert_eq!(s.install_at, Some((3, 30)));
        assert_eq!(s.with(Policy::Auto), vec![Category::Tools]);
        layers.set(nyx_config::Layer::User, "updates", "check", toml::Value::Boolean(false)).unwrap();
        assert_eq!(Settings::from_layers(&layers).with(Policy::Off).len(), 4, "checks off turns everything off");
        assert!(within((3, 30), (3, 45)) && within((23, 30), (0, 10)) && !within((3, 30), (5, 0)));
        assert_eq!(parse_time("24:00"), None);
    }

    #[test]
    fn metered_and_offline_defer_checks() {
        let s = Settings::default();
        assert_eq!(from_nm(70, 2), Network::Online);
        assert_eq!(from_nm(70, 3), Network::Metered);
        assert_eq!(from_nm(20, 0), Network::Offline);
        assert!(deferred(&s, Network::Metered).is_some());
        assert!(deferred(&Settings { check_on_metered: true, ..s.clone() }, Network::Metered).is_none());
        assert!(deferred(&s, Network::Offline).is_some());
        assert!(deferred(&s, Network::Unknown).is_none());
    }
}

//! The update settings (the `updates` domain of the configuration layers) and the
//! network: checking waits for a connection and, unless allowed, for one that isn't
//! metered.

use nyx_config::Layers;

/// `nyx-system config set updates KEY VALUE`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The daily check runs.
    pub check: bool,
    /// It notifies when something new is available.
    pub notify: bool,
    /// It checks on metered connections too.
    pub check_on_metered: bool,
    /// Checks include firmware.
    pub firmware: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { check: true, notify: true, check_on_metered: false, firmware: true }
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
        Settings {
            check: flag("check", d.check),
            notify: flag("notify", d.notify),
            check_on_metered: flag("check-on-metered", d.check_on_metered),
            firmware: flag("firmware", d.firmware),
        }
    }
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

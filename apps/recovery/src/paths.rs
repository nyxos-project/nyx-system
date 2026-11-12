//! Where the system keeps what recovery reads and repairs. Tests point these elsewhere.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    /// The Nix profiles (`system`, `system-N-link`).
    pub profiles: PathBuf,
    /// The System Manager's state: the settings each generation was built from.
    pub state: PathBuf,
    /// The overrides layer (`system.json`).
    pub overrides: PathBuf,
    /// The system's flake, and its nixosConfigurations attribute for this machine.
    pub flake: PathBuf,
    pub host: String,
    pub passwd: PathBuf,
    pub shadow: PathBuf,
    pub group: PathBuf,
    /// Where recovery mounts drives.
    pub mounts: PathBuf,
}

#[derive(serde::Deserialize)]
struct ManagerConfig {
    flake: PathBuf,
    host: String,
}

impl Paths {
    pub fn system() -> Paths {
        let manager: Option<ManagerConfig> =
            std::fs::read_to_string("/etc/nyxos/system.toml").ok().and_then(|t| toml::from_str(&t).ok());
        let (flake, host) = match manager {
            Some(m) => (m.flake, m.host),
            None => ("/etc/nyxos/flake".into(), std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string()),
        };
        Paths {
            profiles: "/nix/var/nix/profiles".into(),
            state: "/var/lib/nyx-system".into(),
            overrides: "/etc/nyxos/overrides".into(),
            flake,
            host,
            passwd: "/etc/passwd".into(),
            shadow: "/etc/shadow".into(),
            group: "/etc/group".into(),
            mounts: "/run/nyx-recovery".into(),
        }
    }

    pub fn profile(&self) -> PathBuf {
        self.profiles.join("system")
    }

    pub fn generation(&self, n: u32) -> PathBuf {
        self.profiles.join(format!("system-{n}-link"))
    }

    pub fn settings_file(&self) -> PathBuf {
        self.overrides.join("system.json")
    }
}

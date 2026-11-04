//! nyx-system-manager: the one place NyxOS changes its own configuration.
//!
//! It runs as root on the system bus, started on demand (D-Bus activation through
//! systemd), and exits when idle. Every method asks polkit whether its caller may do
//! that kind of change, writes an audit line to the journal, and works through the
//! NixOS configuration: system settings live in the overrides layer
//! (`/etc/nyxos/overrides/system.json`), which the NyxOS flake reads as an input, and a
//! change is built before it's switched to. A failed build or switch puts the previous
//! settings back.

mod generations;
mod rebuild;

use anyhow::{Context, Result};
use nyx_system_proto::{BUS_NAME, Change, OBJECT_PATH, SystemSettings, action};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo};

/// `/etc/nyxos/system.toml`, written by the NixOS configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The system's flake.
    pub flake: PathBuf,
    /// Its nixosConfigurations attribute for this machine.
    pub host: String,
    /// The overrides layer (passed to the flake as its `nyxos-overrides` input).
    #[serde(default = "overrides")]
    pub overrides: PathBuf,
    /// Where the manager keeps the settings each generation was built from.
    #[serde(default = "state")]
    pub state: PathBuf,
}

fn overrides() -> PathBuf {
    "/etc/nyxos/overrides".into()
}

fn state() -> PathBuf {
    "/var/lib/nyx-system".into()
}

impl Config {
    fn load() -> Result<Config> {
        let path = std::env::var_os("NYX_SYSTEM_CONFIG").map(PathBuf::from).unwrap_or_else(|| "/etc/nyxos/system.toml".into());
        let text = std::fs::read_to_string(&path).with_context(|| format!("can't read {}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.overrides.join("system.json")
    }

    pub fn read_settings(&self) -> Result<SystemSettings> {
        match std::fs::read_to_string(self.settings_file()) {
            Ok(text) => SystemSettings::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SystemSettings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_settings(&self, s: &SystemSettings) -> Result<()> {
        std::fs::create_dir_all(&self.overrides)?;
        let path = self.settings_file();
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, serde_json::to_string_pretty(s)? + "\n")?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

struct Manager {
    config: Config,
    /// One change at a time.
    busy: Mutex<()>,
    last_use: Arc<std::sync::Mutex<Instant>>,
}

fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

/// The caller's uid, once polkit says it may do `action` (asking the person if needed).
async fn authorize(conn: &Connection, header: &Header<'_>, action: &str) -> fdo::Result<u32> {
    let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
    let dbus = fdo::DBusProxy::new(conn).await?;
    let uid = dbus.get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?).await?;
    let mut subject: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    subject.insert("name", zbus::zvariant::Value::from(sender.as_str()));
    let details: HashMap<&str, &str> = HashMap::new();
    // 1: the person may be asked.
    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(("system-bus-name", subject), action, details, 1u32, ""),
        )
        .await?;
    let (allowed, _challenge, _): (bool, bool, HashMap<String, String>) = reply.body().deserialize()?;
    if !allowed {
        audit(uid, action, "refused", "not authorized");
        return Err(fdo::Error::AccessDenied("not authorized".into()));
    }
    Ok(uid)
}

pub fn audit(uid: u32, action: &str, what: &str, result: &str) {
    eprintln!("audit uid={uid} action={action} {what}: {result}");
}

impl Manager {
    fn touch(&self) {
        *self.last_use.lock().unwrap() = Instant::now();
    }
}

/// People's accounts (uid 1000 to 59999 in /etc/passwd), the only ones a group is given to.
fn people() -> Vec<String> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let uid: u32 = f.get(2)?.parse().ok()?;
            (1000..60000).contains(&uid).then(|| f[0].to_string())
        })
        .collect()
}

#[zbus::interface(name = "io.github.nyxos_project.System1")]
impl Manager {
    async fn settings(&self) -> fdo::Result<String> {
        self.touch();
        let s = self.config.read_settings().map_err(failed)?;
        serde_json::to_string(&s).map_err(failed)
    }

    async fn apply(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        changes: &str,
    ) -> fdo::Result<String> {
        self.touch();
        let changes: Vec<Change> = serde_json::from_str(changes).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        for c in &changes {
            c.validate().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
            if let Change::GroupMember { user, member: true, .. } = c
                && !people().contains(user)
            {
                return Err(fdo::Error::InvalidArgs(format!("{user} isn't a person's account on this computer")));
            }
        }
        let mut actions: Vec<&str> = changes.iter().map(Change::action).collect();
        actions.sort();
        actions.dedup();
        let mut uid = 0;
        for a in &actions {
            uid = authorize(conn, &header, a).await?;
        }
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let summary = serde_json::to_string(&changes).unwrap_or_default();
        let result = rebuild::apply(&self.config, &changes, &emitter).await;
        audit(uid, &actions.join(","), &summary, result.as_ref().map(String::as_str).unwrap_or("failed"));
        self.touch();
        result.map_err(failed)
    }

    async fn set_default(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        domain: &str,
        key: &str,
        value: &str,
    ) -> fdo::Result<()> {
        self.touch();
        let json: serde_json::Value = serde_json::from_str(value).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let value = toml::Value::try_from(json).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let uid = authorize(conn, &header, action::DEFAULTS).await?;
        let layers = nyx_config::Layers { system: self.config.overrides.clone(), ..nyx_config::Layers::standard() };
        let r = layers.set(nyx_config::Layer::System, domain, key, value);
        audit(uid, action::DEFAULTS, &format!("set {domain}.{key}"), if r.is_ok() { "done" } else { "failed" });
        r.map_err(failed)
    }

    async fn reset_default(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        domain: &str,
        key: &str,
    ) -> fdo::Result<()> {
        self.touch();
        let uid = authorize(conn, &header, action::DEFAULTS).await?;
        let layers = nyx_config::Layers { system: self.config.overrides.clone(), ..nyx_config::Layers::standard() };
        let r = layers.reset(nyx_config::Layer::System, domain, key);
        audit(uid, action::DEFAULTS, &format!("reset {domain}.{key}"), if r.is_ok() { "done" } else { "failed" });
        r.map_err(failed)
    }

    async fn update(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        self.touch();
        let uid = authorize(conn, &header, action::UPDATE).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = rebuild::update(&self.config, &emitter).await;
        audit(uid, action::UPDATE, "update", result.as_ref().map(String::as_str).unwrap_or("failed"));
        self.touch();
        result.map_err(failed)
    }

    async fn generations(&self) -> fdo::Result<String> {
        self.touch();
        serde_json::to_string(&generations::list().map_err(failed)?).map_err(failed)
    }

    async fn rollback(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        self.touch();
        let uid = authorize(conn, &header, action::ROLLBACK).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = rebuild::rollback(&self.config, &emitter).await;
        audit(uid, action::ROLLBACK, "rollback", result.as_ref().map(String::as_str).unwrap_or("failed"));
        self.touch();
        result.map_err(failed)
    }

    #[zbus(signal)]
    async fn progress(emitter: &SignalEmitter<'_>, line: &str) -> zbus::Result<()>;
}

/// Exit after this long without a call, unless a change is running.
const IDLE: Duration = Duration::from_secs(300);

/// Whether the running system has another version of the manager: its unit isn't
/// restarted by a switch (it runs the switch), so it exits once idle and the next
/// call starts the new one.
fn stale() -> bool {
    let current = std::fs::canonicalize("/run/current-system/sw/bin/nyx-system-manager");
    let running = std::env::current_exe().and_then(std::fs::canonicalize);
    matches!((current, running), (Ok(c), Ok(r)) if c != r)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let last_use = Arc::new(std::sync::Mutex::new(Instant::now()));
    let manager = Manager { config, busy: Mutex::new(()), last_use: last_use.clone() };
    let conn = zbus::connection::Builder::system()?.name(BUS_NAME)?.serve_at(OBJECT_PATH, manager)?.build().await?;
    // Stale for two ticks in a row, so the reply to the call that switched has gone out.
    let mut stale_ticks = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        stale_ticks = if stale() { stale_ticks + 1 } else { 0 };
        let idle = last_use.lock().unwrap().elapsed() > IDLE;
        let iface = conn.object_server().interface::<_, Manager>(OBJECT_PATH).await?;
        if (idle || stale_ticks >= 2) && iface.get().await.busy.try_lock().is_ok() {
            return Ok(());
        }
    }
}

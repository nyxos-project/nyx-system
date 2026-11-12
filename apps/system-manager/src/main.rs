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
mod snapshots;
mod trust;
mod upgrade;

use anyhow::{Context, Result};
use nyx_system_proto::{BUS_NAME, Change, OBJECT_PATH, SystemSettings, UpgradeRequest, action};
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
    /// One write of the trusted programs' list at a time.
    trust: Mutex<()>,
    last_use: Arc<std::sync::Mutex<Instant>>,
}

/// The error with its causes (`{:#}`), the way the person should read it.
fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

/// Polkit's answer for `action`: allowed, and whether a password would allow it.
async fn check(conn: &Connection, sender: &str, action: &str, interactive: bool) -> fdo::Result<(bool, bool)> {
    let mut subject: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    subject.insert("name", zbus::zvariant::Value::from(sender));
    let details: HashMap<&str, &str> = HashMap::new();
    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            // 1: the person may be asked.
            &(("system-bus-name", subject), action, details, u32::from(interactive), ""),
        )
        .await?;
    let (allowed, challenge, _): (bool, bool, HashMap<String, String>) = reply.body().deserialize()?;
    Ok((allowed, challenge))
}

/// The caller's uid, once polkit says it may do every one of `actions`. One change of
/// several kinds asks once: each is checked without asking (what polkit denies stays
/// denied), and what still needs a password is asked for together, as `system.apply`.
async fn authorize_all(conn: &Connection, header: &Header<'_>, actions: &[&str]) -> fdo::Result<u32> {
    if let [one] = actions {
        return authorize(conn, header, one).await;
    }
    let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
    let dbus = fdo::DBusProxy::new(conn).await?;
    let uid = dbus.get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?).await?;
    let mut pending = Vec::new();
    for a in actions {
        match check(conn, &sender, a, false).await? {
            (true, _) => {}
            (false, true) => pending.push(*a),
            (false, false) => {
                audit(uid, a, "refused", "not authorized");
                return Err(fdo::Error::AccessDenied("not authorized".into()));
            }
        }
    }
    if !pending.is_empty() && !check(conn, &sender, action::APPLY, true).await?.0 {
        audit(uid, &pending.join(","), "refused", "not authorized");
        return Err(fdo::Error::AccessDenied("not authorized".into()));
    }
    Ok(uid)
}

/// The caller's uid, once polkit says it may do `action` (asking the person if needed).
async fn authorize(conn: &Connection, header: &Header<'_>, action: &str) -> fdo::Result<u32> {
    let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
    let dbus = fdo::DBusProxy::new(conn).await?;
    let uid = dbus.get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?).await?;
    if !check(conn, &sender, action, true).await?.0 {
        audit(uid, action, "refused", "not authorized");
        return Err(fdo::Error::AccessDenied("not authorized".into()));
    }
    Ok(uid)
}

/// Whether the caller may read the settings, history and generations: root, an
/// administrator (their update timer runs outside a session), or someone polkit allows
/// (an active local session).
async fn may_read(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
    let dbus = fdo::DBusProxy::new(conn).await?;
    let uid = dbus.get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?).await?;
    if uid == 0 || administrator(uid, &std::fs::read_to_string("/etc/passwd").unwrap_or_default(), &std::fs::read_to_string("/etc/group").unwrap_or_default()) {
        return Ok(());
    }
    if check(conn, &sender, action::READ, false).await?.0 {
        return Ok(());
    }
    Err(fdo::Error::AccessDenied("only people signed in at this computer can read the system's settings".into()))
}

/// Whether `uid` is in the wheel group (as the group's member list or its own group).
fn administrator(uid: u32, passwd: &str, group: &str) -> bool {
    let Some((name, gid)) = passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 3 && f[2].parse() == Ok(uid)).then(|| (f[0], f[3]))
    }) else {
        return false;
    };
    group.lines().any(|l| {
        let f: Vec<&str> = l.split(':').collect();
        f.len() > 3 && f[0] == "wheel" && (f[2] == gid || f[3].split(',').any(|m| m == name))
    })
}

#[cfg(test)]
#[test]
fn administrators_are_in_wheel() {
    let passwd = "root:x:0:0::/root:/bin/sh\nken:x:1000:100::/home/ken:/bin/zsh\namy:x:1001:100::/home/amy:/bin/zsh\n";
    let group = "wheel:x:1:ken\nusers:x:100:\n";
    assert!(administrator(1000, passwd, group));
    assert!(!administrator(1001, passwd, group));
    assert!(!administrator(4242, passwd, group));
}

pub fn audit(uid: u32, action: &str, what: &str, result: &str) {
    eprintln!("audit uid={uid} action={action} {what}: {result}");
}

impl Manager {
    fn touch(&self) {
        *self.last_use.lock().unwrap() = Instant::now();
    }

    /// Build and switch to `changes`, already authorized for `actions`.
    async fn apply_changes(
        &self,
        uid: u32,
        changes: &[Change],
        actions: &[&str],
        emitter: &SignalEmitter<'_>,
    ) -> anyhow::Result<String> {
        let summary = serde_json::to_string(changes).unwrap_or_default();
        let result = rebuild::apply(&self.config, changes, emitter).await;
        if result.is_ok() {
            for c in changes {
                if let Change::RemoveMount { id } = c {
                    let _ = std::fs::remove_file(std::path::Path::new(nyx_system_proto::MOUNT_LOGINS).join(id));
                }
            }
        }
        audit(uid, &actions.join(","), &summary, result.as_ref().map(String::as_str).unwrap_or("failed"));
        result
    }

    /// The whole channel, already authorized: what `Update` has always done.
    async fn channel_update(&self, uid: u32, request: &str, emitter: &SignalEmitter<'_>) -> anyhow::Result<String> {
        let req = UpgradeRequest { channel: true, tools: Vec::new(), request: request.into() };
        let result = upgrade::upgrade(&self.config, &req, emitter).await.map(|r| r.summary);
        audit(uid, action::UPDATE, "update", result.as_ref().map(String::as_str).unwrap_or("failed"));
        result
    }
}

/// A client's JSON changes, validated, with what the computer has found by the manager
/// itself (never taken from a client).
fn checked_changes(json: &str) -> fdo::Result<Vec<Change>> {
    let mut changes: Vec<Change> = serde_json::from_str(json).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
    for c in &mut changes {
        if let Change::Hardware { hardware } = c {
            *hardware = nyx_system_proto::hardware::detect(std::path::Path::new("/"));
        }
    }
    for c in &changes {
        c.validate().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        check_against_system(c).map_err(fdo::Error::InvalidArgs)?;
    }
    Ok(changes)
}

/// The polkit actions `changes` need, each once.
fn actions_for(changes: &[Change]) -> Vec<&'static str> {
    let mut actions: Vec<&str> = changes.iter().map(Change::action).collect();
    actions.sort();
    actions.dedup();
    actions
}

/// Accounts in /etc/passwd: name, uid and shell.
fn accounts() -> Vec<(String, u32, String)> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.first()?.to_string(), f.get(2)?.parse().ok()?, f.get(6).unwrap_or(&"").to_string()))
        })
        .collect()
}

/// A person's account: a uid from 1000 to 59999 and a shell to sign in with (NixOS's
/// build users have uids there too, but no shell).
fn is_person(uid: u32, shell: &str) -> bool {
    (1000..60000).contains(&uid) && !shell.ends_with("/nologin") && !shell.ends_with("/false")
}

/// People's accounts, the only ones a group is given to or a password set for.
fn people() -> Vec<String> {
    accounts().into_iter().filter(|(_, uid, shell)| is_person(*uid, shell)).map(|(n, _, _)| n).collect()
}

/// Accounts the NixOS configuration declares (the only ones it can rename).
fn declared() -> Vec<String> {
    std::fs::read_to_string("/var/lib/nixos/declarative-users").unwrap_or_default().split_whitespace().map(String::from).collect()
}

fn valid_password(password: &str) -> Result<(), fdo::Error> {
    if password.is_empty() || password.len() > 512 || password.contains(['\n', '\r', '\0']) {
        return Err(fdo::Error::InvalidArgs("a password is 1 to 512 characters on one line".into()));
    }
    Ok(())
}

/// Set a password with chpasswd; it never touches the configuration.
async fn chpasswd(user: &str, password: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("chpasswd")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(format!("{user}:{password}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Checks against the running system that the changes themselves can't make.
fn check_against_system(c: &Change) -> Result<(), String> {
    match c {
        Change::GroupMember { user, member: true, .. } if !people().contains(user) => {
            Err(format!("{user} isn't a person's account on this computer"))
        }
        Change::AddPerson { name, .. } if accounts().iter().any(|(n, _, _)| n == name) => {
            Err(format!("there's already an account called {name}"))
        }
        Change::FullName { user, .. } if !people().contains(user) || !declared().contains(user) => {
            Err(format!("{user}'s name isn't one NyxOS's configuration sets"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
#[test]
fn build_users_arent_people() {
    assert!(is_person(1000, "/run/current-system/sw/bin/zsh"));
    assert!(!is_person(30001, "/run/current-system/sw/bin/nologin"));
    assert!(!is_person(65534, "/run/current-system/sw/bin/zsh"));
}

#[zbus::interface(name = "io.github.nyxos_project.System1")]
impl Manager {
    async fn settings(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<String> {
        may_read(conn, &header).await?;
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
        let changes = checked_changes(changes)?;
        let actions = actions_for(&changes);
        let uid = authorize_all(conn, &header, &actions).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = self.apply_changes(uid, &changes, &actions, &emitter).await;
        self.touch();
        result.map_err(failed)
    }

    async fn apply_and_update(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        changes: &str,
    ) -> fdo::Result<String> {
        self.touch();
        let changes = checked_changes(changes)?;
        let mut actions = actions_for(&changes);
        actions.push(action::UPDATE);
        let uid = authorize_all(conn, &header, &actions).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let mut summary = Vec::new();
        if !changes.is_empty() {
            actions.pop();
            summary.push(self.apply_changes(uid, &changes, &actions, &emitter).await.map_err(failed)?);
        }
        let result = self.channel_update(uid, "update (with setup)", &emitter).await;
        self.touch();
        summary.push(result.map_err(failed)?);
        Ok(summary.join("\n"))
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
        let result = self.channel_update(uid, "update", &emitter).await;
        self.touch();
        result.map_err(failed)
    }

    /// The channel and/or single tools in one rebuild (JSON `UpgradeRequest`); returns
    /// JSON `UpgradeResult`.
    async fn upgrade(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        request: &str,
    ) -> fdo::Result<String> {
        self.touch();
        let request: UpgradeRequest = serde_json::from_str(request).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        request.validate().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let uid = authorize(conn, &header, action::UPDATE).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = upgrade::upgrade(&self.config, &request, &emitter).await;
        let what = if request.channel { "upgrade channel".to_string() } else { format!("upgrade {}", request.tools.join(",")) };
        audit(uid, action::UPDATE, &what, result.as_ref().map(|r| r.summary.as_str()).unwrap_or("failed"));
        self.touch();
        serde_json::to_string(&result.map_err(failed)?).map_err(failed)
    }

    async fn history(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection, limit: u32) -> fdo::Result<String> {
        may_read(conn, &header).await?;
        self.touch();
        let list = nyx_system_proto::read_history(&upgrade::history_file(&self.config), limit.min(1000) as usize);
        serde_json::to_string(&list).map_err(failed)
    }

    async fn generations(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<String> {
        may_read(conn, &header).await?;
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
        let result = upgrade::rollback(&self.config, &emitter).await;
        audit(uid, action::ROLLBACK, "rollback", result.as_ref().map(String::as_str).unwrap_or("failed"));
        self.touch();
        result.map_err(failed)
    }

    /// Mount a drive or share at startup, keeping a share's login apart (root only).
    async fn add_mount(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        id: &str,
        mount: &str,
        login: &str,
    ) -> fdo::Result<String> {
        self.touch();
        let mut mount: nyx_system_proto::Mount =
            serde_json::from_str(mount).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        mount.login = !login.is_empty();
        let change = Change::AddMount { id: id.into(), mount };
        change.validate().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let login_ok = login.len() < 2048
            && login.lines().all(|l| ["username=", "password=", "domain="].iter().any(|k| l.starts_with(k)) && !l.contains('\0'));
        if !login_ok {
            return Err(fdo::Error::InvalidArgs("a login is username=, password= and domain= lines".into()));
        }
        let uid = authorize(conn, &header, action::MOUNTS).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let file = std::path::Path::new(nyx_system_proto::MOUNT_LOGINS).join(id);
        if !login.is_empty() {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(nyx_system_proto::MOUNT_LOGINS).map_err(failed)?;
            let mut f =
                std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&file).map_err(failed)?;
            std::io::Write::write_all(&mut f, format!("{}\n", login.trim_end()).as_bytes()).map_err(failed)?;
        }
        let result = rebuild::apply(&self.config, std::slice::from_ref(&change), &emitter).await;
        audit(uid, action::MOUNTS, &format!("add mount {id}"), result.as_ref().map(String::as_str).unwrap_or("failed"));
        if result.is_err() {
            let _ = std::fs::remove_file(&file);
        }
        self.touch();
        result.map_err(failed)
    }

    /// Add an account and set its password, asked for once.
    #[allow(clippy::too_many_arguments)]
    async fn add_person(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        name: &str,
        full_name: &str,
        admin: bool,
        password: &str,
    ) -> fdo::Result<String> {
        self.touch();
        let change = Change::AddPerson { name: name.into(), full_name: full_name.into(), admin };
        change.validate().map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        check_against_system(&change).map_err(fdo::Error::InvalidArgs)?;
        valid_password(password)?;
        let uid = authorize(conn, &header, action::ACCOUNTS).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = rebuild::apply(&self.config, std::slice::from_ref(&change), &emitter).await;
        audit(uid, action::ACCOUNTS, &format!("add {name}"), result.as_ref().map(String::as_str).unwrap_or("failed"));
        let summary = result.map_err(failed)?;
        chpasswd(name, password)
            .await
            .map_err(|e| failed(format!("{name}'s account was added, but its password couldn't be set: {e}")))?;
        audit(uid, action::ACCOUNTS, &format!("password for {name}"), "done");
        self.touch();
        Ok(summary)
    }

    async fn set_password(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        user: &str,
        password: &str,
    ) -> fdo::Result<()> {
        self.touch();
        let Some(target) =
            accounts().into_iter().find(|(n, uid, shell)| n == user && is_person(*uid, shell)).map(|(_, uid, _)| uid)
        else {
            return Err(fdo::Error::InvalidArgs(format!("{user} isn't a person's account on this computer")));
        };
        valid_password(password)?;
        let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
        let caller = fdo::DBusProxy::new(conn)
            .await?
            .get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?)
            .await?;
        let act = if caller == target { action::PASSWORD } else { action::ACCOUNTS };
        let uid = authorize(conn, &header, act).await?;
        let r = chpasswd(user, password).await;
        audit(uid, act, &format!("password for {user}"), if r.is_ok() { "done" } else { "failed" });
        r.map_err(failed)
    }

    async fn collect_garbage(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        older_than_days: u32,
    ) -> fdo::Result<String> {
        self.touch();
        let uid = authorize(conn, &header, action::CLEAN).await?;
        let _guard = self.busy.try_lock().map_err(|_| failed("another system change is running"))?;
        let result = rebuild::collect_garbage(&self.config, older_than_days, &emitter).await;
        audit(
            uid,
            action::CLEAN,
            &format!("collect-garbage {older_than_days}d"),
            result.as_ref().map(String::as_str).unwrap_or("failed"),
        );
        self.touch();
        result.map_err(failed)
    }

    async fn vacuum_journal(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        megabytes: u32,
    ) -> fdo::Result<String> {
        self.touch();
        let uid = authorize(conn, &header, action::CLEAN).await?;
        let result = rebuild::vacuum_journal(&self.config, megabytes, &emitter).await;
        audit(
            uid,
            action::CLEAN,
            &format!("vacuum-journal {megabytes}M"),
            result.as_ref().map(String::as_str).unwrap_or("failed"),
        );
        self.touch();
        result.map_err(failed)
    }

    /// Trust the Nyx apps in the caller's own Nix profile that NyxOS's cache built, like
    /// the system's (`/etc/nyx/trusted-bin`). Anyone may ask, for their own profile only;
    /// returns how many apps that is.
    async fn trust_apps(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<u32> {
        self.touch();
        let sender = header.sender().ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?.to_string();
        let uid = fdo::DBusProxy::new(conn)
            .await?
            .get_connection_unix_user(zbus::names::BusName::try_from(sender.as_str()).map_err(failed)?)
            .await?;
        let home = trust::home(uid).ok_or_else(|| failed(format!("uid {uid} has no home folder")))?;
        let _guard = self.trust.lock().await;
        let state = self.config.state.clone();
        let n = tokio::task::spawn_blocking(move || trust::refresh(&state, uid, &home)).await.map_err(failed)?.map_err(failed)?;
        audit(uid, "trust-apps", "profile apps", &format!("{n} trusted"));
        Ok(n as u32)
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
    upgrade::recover_interrupted(&config);
    rebuild::recover_staged(&config);
    let last_use = Arc::new(std::sync::Mutex::new(Instant::now()));
    let manager = Manager { config, busy: Mutex::new(()), trust: Mutex::new(()), last_use: last_use.clone() };
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

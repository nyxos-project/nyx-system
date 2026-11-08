//! The theme service on a private session bus. Needs a bus of its own, so it is
//! ignored by default; run it with
//!
//!     dbus-run-session -- cargo test -p nyx-themed --test dbus -- --ignored
//!
//! Everything runs in scratch XDG folders with the desktop outputs off.

use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;
use tokio::time::timeout;

const BUS: &str = "io.github.design_nexus.nyx.Theme";
const PATH: &str = "/io/github/design_nexus/nyx/Theme";
const IFACE: &str = "io.github.design_nexus.nyx.Theme1";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Scratch {
    let root = std::env::temp_dir().join(format!("nyx-themed-dbus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["config", "data", "state", "runtime", "cache"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    Scratch(root)
}

fn service(root: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_nyx-themed"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_DIRS", root.join("none"))
        .env("NYX_THEMED_OUTPUTS", "0")
        .env("NYX_REDUCE_MOTION", "0")
        .env("NYX_HIGH_CONTRAST", "0")
        .spawn()
        .expect("nyx-themed starts")
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_for_name(conn: &zbus::Connection) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    for _ in 0..100 {
        if dbus.name_has_owner(BUS.try_into().unwrap()).await.unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the service never took its name");
}

async fn proxy(conn: &zbus::Connection) -> zbus::Proxy<'static> {
    zbus::Proxy::new(conn, BUS, PATH, IFACE).await.unwrap()
}

type Event = (u64, String, String, String, u32);

async fn next(s: &mut zbus::proxy::SignalStream<'static>) -> Event {
    let m = timeout(Duration::from_secs(5), s.next()).await.expect("a ThemeChanged in time").unwrap();
    m.body().deserialize::<Event>().unwrap()
}

fn snapshot(root: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(root.join("runtime/nyx/theme/system.json")).unwrap()).unwrap()
}

#[tokio::test]
#[ignore = "needs its own session bus: dbus-run-session -- cargo test -p nyx-themed --test dbus -- --ignored"]
async fn live_switching_overrides_and_restarts() {
    let s = scratch();
    let root = s.0.clone();
    let mut svc = Running(service(&root));
    let conn = zbus::Connection::session().await.unwrap();
    wait_for_name(&conn).await;
    let p = proxy(&conn).await;
    let active: String = p.call("GetActiveTheme", &()).await.unwrap();
    assert_eq!(active, "catppuccin-mocha", "the default theme");
    let start: u64 = p.call("GetGeneration", &()).await.unwrap();
    assert!(start >= 1, "the service publishes a snapshot when it starts");

    // Two clients hear every change.
    let other = zbus::Connection::session().await.unwrap();
    let p2 = proxy(&other).await;
    let mut a = p.receive_signal("ThemeChanged").await.unwrap();
    let mut b = p2.receive_signal("ThemeChanged").await.unwrap();

    let () = p.call("SetSystemTheme", &("nord",)).await.unwrap();
    let ea = next(&mut a).await;
    let eb = next(&mut b).await;
    assert_eq!((ea.1.as_str(), eb.1.as_str()), ("nord", "nord"));
    assert!(ea.0 > start && ea.0 == eb.0, "one generation, the same for everyone");
    assert_eq!(snapshot(&root)["theme"]["id"], "nord");
    assert_eq!(snapshot(&root)["generation"].as_u64(), Some(ea.0));

    // A theme that doesn't exist is refused; nothing changes.
    assert!(p.call::<_, _, ()>("SetSystemTheme", &("no-such-theme",)).await.is_err());
    assert_eq!(snapshot(&root)["theme"]["id"], "nord");

    // Another program writing the choices file is noticed.
    std::fs::write(root.join("config/nyx/theme.toml"), "mode = \"theme\"\ntheme = \"dracula\"\n").unwrap();
    let e = next(&mut a).await;
    assert_eq!(e.1, "dracula");
    assert_eq!(e.3, "files-changed");

    // Rapid switching ends on the last choice, generations only going up.
    let mut last = e.0;
    for t in ["kanagawa", "gruvbox-dark", "rose-pine"] {
        let () = p.call("SetSystemTheme", &(t,)).await.unwrap();
    }
    loop {
        let e = next(&mut a).await;
        assert!(e.0 > last, "generations go up ({} after {last})", e.0);
        last = e.0;
        if e.1 == "rose-pine" {
            break;
        }
    }
    assert_eq!(snapshot(&root)["theme"]["id"], "rose-pine");

    // An app override changes that app only.
    let () = p.call("SetAppOverride", &("nyx-files", "dracula")).await.unwrap();
    let e = next(&mut a).await;
    assert_eq!(e.1, "rose-pine", "the system theme stays");
    let files: String = p.call("ResolveEffectiveTheme", &("nyx-files",)).await.unwrap();
    let files: serde_json::Value = serde_json::from_str(&files).unwrap();
    assert_eq!(files["theme"]["id"], "dracula");
    let mail: String = p.call("ResolveEffectiveTheme", &("nyx-mail",)).await.unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&mail).unwrap()["theme"]["id"], "rose-pine");

    // A restarted service picks up where it was; a reconnecting client asks for the current generation.
    let before: u64 = p.call("GetGeneration", &()).await.unwrap();
    drop(svc);
    svc = Running(service(&root));
    wait_for_name(&conn).await;
    let p = proxy(&conn).await;
    let after: u64 = p.call("GetGeneration", &()).await.unwrap();
    assert!(after >= before, "generations survive a restart ({after} < {before})");
    let active: String = p.call("GetActiveTheme", &()).await.unwrap();
    assert_eq!(active, "rose-pine");
    let themes: String = p.call("ListThemes", &()).await.unwrap();
    assert!(themes.contains("\"catppuccin-mocha\""));
    drop(svc);
}

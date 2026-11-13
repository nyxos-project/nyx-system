//! nyx-themed: the theme service. It owns `io.github.design_nexus.nyx.Theme1` on the
//! session bus and keeps every running client on the same theme:
//!
//! - it watches the theme choices (`~/.config/nyx/theme.toml`), the system's default
//!   theme (NyxOS's `appearance` defaults and overrides), theme files, installed themes
//!   and, on Omarchy, the active Omarchy theme;
//! - on a change it resolves the system theme once, writes the immutable snapshot
//!   (`$XDG_RUNTIME_DIR/nyx/theme/system.json`) as the next generation and emits
//!   `ThemeChanged`; clients fetch or resolve the new theme themselves;
//! - it carries the theme to what isn't a Nyx app: Hyprland's colours
//!   (`~/.config/hypr/nyx-theme.lua`, then `hyprctl reload`), Foot's
//!   (`~/.config/foot/nyx-theme.ini`), the zsh prompt's (`~/.config/zsh/nyx-theme.zsh`),
//!   GTK's light or dark preference, the wallpaper
//!   (the user's own, else the theme's default), the lock screen and the Umbra and Vellum icons.
//!
//! The choices file is the source of truth, so apps keep working without this
//! service; it only makes changes arrive at once and in step.
//!
//! `--once` applies everything and exits. `--no-outputs` (or `NYX_THEMED_OUTPUTS=0`)
//! leaves the desktop files alone, for tests.

mod outputs;
mod service;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let outputs_on = !args.iter().any(|a| a == "--no-outputs") && std::env::var("NYX_THEMED_OUTPUTS").as_deref() != Ok("0");
    if args.iter().any(|a| a == "--once") {
        service::refresh(outputs_on, "start");
        return Ok(());
    }
    let state = service::State::new(outputs_on);
    let svc = service::Service { state: state.clone() };
    let conn = zbus::connection::Builder::session()?
        .name(nyx_theme_core::dbus::BUS_NAME)?
        .serve_at(nyx_theme_core::dbus::PATH, svc)?
        .build()
        .await?;
    // Holding the bus name, this is the theme service: the snapshot's only writer.
    if let Err(e) = nyx_theme_core::snapshot::claim_service() {
        eprintln!("nyx-themed: can't claim the snapshot: {e}");
    }
    service::changed(&conn, &state, "start").await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
        // Reading the files (as this service and every Nyx app does) is no change;
        // staging and rollback copies are the installer's business.
        if let Ok(e) = e
            && !matches!(e.kind, notify::EventKind::Access(_))
            && e.paths.iter().any(|p| {
                let s = p.to_string_lossy();
                // In /etc itself only a NixOS switch replacing /etc/static counts.
                let etc = p.parent() == Some(std::path::Path::new("/etc"));
                (!etc || p.file_name().is_some_and(|n| n == "static"))
                    && !s.contains("/.staging")
                    && !s.contains("/.previous")
                    && !s.ends_with(".lock")
                    && !s.contains(".tmp-")
            })
        {
            let _ = tx.send(());
        }
    })?;
    let installed = nyx_theme_core::paths::installed_dir();
    let _ = std::fs::create_dir_all(nyx_theme_core::paths::theme_dirs()[0].as_path());
    let _ = std::fs::create_dir_all(&installed);
    for dir in nyx_theme_core::paths::watched_dirs() {
        // Installed themes live in a folder each; the Omarchy folders exist only on Omarchy.
        let mode = if dir == installed { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
        if let Err(e) = watcher.watch(&dir, mode)
            && dir.exists()
        {
            eprintln!("nyx-themed: can't watch {}: {e}", dir.display());
        }
    }
    // The system's default theme (/etc/nyxos/defaults/appearance.toml) points through
    // /etc/static, which a NixOS switch replaces without touching the file's own link.
    if std::path::Path::new("/etc/static").exists() {
        let _ = watcher.watch(std::path::Path::new("/etc"), RecursiveMode::NonRecursive);
    }
    while rx.recv().await.is_some() {
        // A change arrives in steps (write, rename); take the last of a burst.
        tokio::time::sleep(Duration::from_millis(250)).await;
        while rx.try_recv().is_ok() {}
        service::changed(&conn, &state, "files-changed").await;
    }
    Ok(())
}

//! nyx-themed: carries the Nyx theme the user chose (`~/.config/nyx/theme.toml`, the
//! file every Nyx app follows) to the rest of the desktop, live:
//!
//! - Hyprland's border and background colours (`~/.config/hypr/nyx-theme.lua`, which
//!   NyxOS's `hyprland.lua` loads), then `hyprctl reload`;
//! - Foot's colours (`~/.config/foot/nyx-theme.ini`, included by `foot.ini`; Helix uses
//!   the terminal's palette);
//! - GTK's light or dark preference;
//! - the wallpaper: the user's own (`appearance.toml`'s `wallpaper`), else the theme's
//!   default from the Nyx wallpapers, shown by `nyx-wallpaper.service`;
//! - the Umbra icons, redrawn in the theme's colours (`~/.local/share/icons`).
//!
//! `--once` writes everything and exits; without it, it also follows changes.

mod outputs;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

fn apply_all() {
    let theme = nyx_theme::current_theme();
    let appearance = nyx_config::Layers::standard().merged("appearance").unwrap_or_default();
    for (what, result) in outputs::write_all(&theme, &appearance) {
        if let Err(e) = result {
            eprintln!("nyx-themed: {what}: {e:#}");
        }
    }
    eprintln!("nyx-themed: {} applied", theme.id);
}

fn main() -> Result<()> {
    apply_all();
    if std::env::args().any(|a| a == "--once") {
        return Ok(());
    }
    let dir = config_home().join("nyx");
    std::fs::create_dir_all(&dir)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
        if let Ok(e) = e
            && e.paths.iter().any(|p| p.file_name().is_some_and(|n| n == "theme.toml" || n == "appearance.toml"))
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    // The Omarchy theme's colours, when an app follows them, come and go with it too.
    let _ = watcher.watch(&nyx_config_state().join("omarchy/current"), RecursiveMode::NonRecursive);
    while rx.recv().is_ok() {
        // An app writes the file in steps; take the last of a burst.
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        apply_all();
    }
    Ok(())
}

fn nyx_config_state() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
}

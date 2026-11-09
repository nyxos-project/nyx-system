//! Being told when a domain's settings change, in any layer.

use crate::store::Store;
use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Watches until dropped.
pub struct Watch {
    _watcher: notify::RecommendedWatcher,
}

impl Store {
    /// Calls `on_change` (on a background thread) whenever the domain's file changes in
    /// any layer, including a NixOS switch replacing `/etc/static` under the defaults. The
    /// user's folder is created so it can be watched; a layer folder that doesn't exist
    /// isn't watched. Changes the component made itself arrive too: compare before acting.
    pub fn watch(&self, on_change: impl Fn() + Send + 'static) -> Result<Watch> {
        let _ = std::fs::create_dir_all(&self.layers.user);
        let file: OsString = format!("{}.toml", self.domain.name).into();
        let dirs: Vec<PathBuf> = [&self.layers.defaults, &self.layers.system, &self.layers.user]
            .into_iter()
            .filter(|d| d.is_dir())
            .cloned()
            .collect();
        // /etc/nyxos/defaults/* point through /etc/static, which a switch replaces.
        let etc_static = self.layers.defaults.starts_with("/etc").then(|| PathBuf::from("/etc"));
        let relevant = move |p: &Path| {
            p.file_name() == Some(file.as_os_str())
                || (etc_static.is_some() && p.parent() == Some(Path::new("/etc")) && p.file_name() == Some("static".as_ref()))
        };
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if event.paths.iter().any(|p| relevant(p)) {
                on_change();
            }
        })?;
        for dir in &dirs {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }
        if self.layers.defaults.starts_with("/etc") && Path::new("/etc").is_dir() {
            let _ = watcher.watch(Path::new("/etc"), RecursiveMode::NonRecursive);
        }
        Ok(Watch { _watcher: watcher })
    }
}

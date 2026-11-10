//! The last check, kept for when there's no network (what it found, and when) and for
//! Settings' "last checked".

use crate::{Firmware, Status, Target};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checked {
    /// Seconds since the epoch.
    pub time: i64,
    pub status: Status,
    pub targets: Vec<Target>,
    pub firmware: Firmware,
}

/// `$XDG_CACHE_HOME/nyx/update-status.json`.
pub fn path() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    cache.join("nyx/update-status.json")
}

pub fn save(c: &Checked) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(c) {
        let tmp = p.with_extension("json.new");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

pub fn load() -> Option<Checked> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

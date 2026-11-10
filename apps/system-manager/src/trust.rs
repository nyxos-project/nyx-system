//! The Nyx apps people install for themselves, trusted like the system's. Services that
//! decide what a caller may do by its program (nyx-peer: Accounts, Clipboard, app
//! actions, Voice, the shell) read [`LIST`], which only root writes. It's the system's
//! list ([`SYSTEM_LIST`], from the NixOS configuration) and the `bin` folders of Nyx
//! apps in people's Nix profiles that NyxOS's own cache signed, so nobody adds a program
//! of their own by building something called nyx-mail.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const SYSTEM_LIST: &str = "/etc/nyx/trusted-bin.system";
pub const LIST: &str = "/etc/nyx/trusted-bin";

/// The signing keys whose builds count: NyxOS's binary cache. Nix only takes a
/// signature from a trusted cache, and only trusted users can add one.
const KEYS: &[&str] = &["nyxos.cachix.org-1"];

/// `UID DIR` lines: what each person's profile added.
fn state_file(state: &Path) -> PathBuf {
    state.join("trusted-profile-bin")
}

/// The store paths in the Nix profile under `home`, when the profile is in the store.
pub fn profile_store_paths(home: &Path) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    for link in [home.join(".local/state/nix/profiles/profile"), home.join(".nix-profile")] {
        let Ok(profile) = std::fs::canonicalize(&link) else { continue };
        if !profile.starts_with("/nix/store") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(profile.join("manifest.json")) else { continue };
        out.extend(manifest_paths(&text));
    }
    out.into_iter().collect()
}

/// The store paths a profile's `manifest.json` lists (version 2's array, 3's map).
pub fn manifest_paths(text: &str) -> Vec<PathBuf> {
    let Ok(m) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let elements: Vec<&serde_json::Value> = match &m["elements"] {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(o) => o.values().collect(),
        _ => Vec::new(),
    };
    elements
        .iter()
        .flat_map(|e| e["storePaths"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p.as_str().map(PathBuf::from))
        .filter(|p| is_nyx_app(p))
        .collect()
}

/// `/nix/store/<hash>-nyx-…`.
fn is_nyx_app(p: &Path) -> bool {
    let Ok(rest) = p.strip_prefix("/nix/store") else { return false };
    let Some(name) = rest.to_str().filter(|n| !n.contains('/')) else { return false };
    matches!(name.split_once('-'), Some((hash, after)) if hash.len() == 32 && after.starts_with("nyx-"))
}

/// Whether one of [`KEYS`] signed `path`, as `nix path-info --json --sigs` says.
fn signed(path: &Path) -> bool {
    let Ok(out) = std::process::Command::new("nix").args(["path-info", "--json", "--sigs"]).arg(path).output() else {
        return false;
    };
    out.status.success() && signatures_ok(&String::from_utf8_lossy(&out.stdout))
}

/// `nix path-info --json`'s signatures (an object by path, or an older array) include
/// one of [`KEYS`].
fn signatures_ok(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return false };
    let infos: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(o) => o.values().collect(),
        _ => Vec::new(),
    };
    infos
        .iter()
        .flat_map(|i| i["signatures"].as_array().cloned().unwrap_or_default())
        .any(|s| s.as_str().and_then(|s| s.split_once(':')).is_some_and(|(key, _)| KEYS.contains(&key)))
}

/// Replace what `uid`'s profile added with its signed Nyx apps now, then write [`LIST`].
/// Returns how many folders that person's profile adds.
pub fn refresh(state: &Path, uid: u32, home: &Path) -> Result<usize> {
    let dirs: Vec<String> = profile_store_paths(home)
        .into_iter()
        .filter(|p| p.join("bin").is_dir() && signed(p))
        .map(|p| format!("{}/bin", p.display()))
        .collect();
    let file = state_file(state);
    let mut lines: Vec<String> = std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split_once(' ').is_some_and(|(u, _)| u != uid.to_string()))
        .map(String::from)
        .collect();
    lines.extend(dirs.iter().map(|d| format!("{uid} {d}")));
    std::fs::create_dir_all(state)?;
    write(&file, &lines.join("\n"))?;
    compose(state, Path::new(SYSTEM_LIST), Path::new(LIST))?;
    Ok(dirs.len())
}

/// [`LIST`]: the system's folders and every person's, the ones still there.
pub fn compose(state: &Path, system: &Path, list: &Path) -> Result<()> {
    let mut dirs: Vec<String> = std::fs::read_to_string(system).unwrap_or_default().lines().map(String::from).collect();
    for l in std::fs::read_to_string(state_file(state)).unwrap_or_default().lines() {
        if let Some((_, d)) = l.split_once(' ')
            && Path::new(d).is_dir()
            && !dirs.iter().any(|x| x == d)
        {
            dirs.push(d.to_string());
        }
    }
    write(list, &dirs.join("\n"))
}

/// Written whole, owned by root and readable by all (nyx-peer refuses a list others can
/// write).
fn write(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, format!("{text}\n")).with_context(|| tmp.display().to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
    Ok(())
}

/// A person's home folder, from /etc/passwd.
pub fn home(uid: u32) -> Option<PathBuf> {
    std::fs::read_to_string("/etc/passwd").ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        if f.get(2)?.parse::<u32>().ok()? != uid {
            return None;
        }
        f.get(5).map(PathBuf::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdfghijklmnpqrsvwxyz";

    #[test]
    fn only_nyx_apps_from_the_manifest() {
        let v3 = format!(
            r#"{{"version":3,"elements":{{"nyx-mail":{{"storePaths":["/nix/store/{HASH}-nyx-mail-0.1.0"]}},"firefox":{{"storePaths":["/nix/store/{HASH}-firefox-140"]}}}}}}"#
        );
        assert_eq!(manifest_paths(&v3), vec![PathBuf::from(format!("/nix/store/{HASH}-nyx-mail-0.1.0"))]);
        let v2 = format!(r#"{{"version":2,"elements":[{{"storePaths":["/nix/store/{HASH}-nyx-notes-1"]}}]}}"#);
        assert_eq!(manifest_paths(&v2).len(), 1);
        assert!(!is_nyx_app(Path::new("/home/x/nyx-mail")));
        assert!(!is_nyx_app(Path::new("/nix/store/short-nyx-mail")));
        assert!(!is_nyx_app(Path::new(&format!("/nix/store/{HASH}-nyx-mail/bin"))));
    }

    #[test]
    fn only_nyxos_signatures_count() {
        assert!(signatures_ok(r#"{"/nix/store/x":{"signatures":["nyxos.cachix.org-1:abc"]}}"#));
        assert!(signatures_ok(r#"[{"path":"/nix/store/x","signatures":["cache.nixos.org-1:a","nyxos.cachix.org-1:b"]}]"#));
        assert!(!signatures_ok(r#"{"/nix/store/x":{"signatures":["cache.nixos.org-1:abc"]}}"#));
        assert!(!signatures_ok(r#"{"/nix/store/x":{"ultimate":true,"signatures":[]}}"#));
        assert!(!signatures_ok("not json"));
    }

    #[test]
    fn the_list_is_the_systems_and_each_persons() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app/bin");
        std::fs::create_dir_all(&app).unwrap();
        let system = dir.path().join("system");
        std::fs::write(&system, "/nix/store/a-nyx-files/bin\n").unwrap();
        std::fs::write(state_file(dir.path()), format!("1000 {}\n1001 /nix/store/gone/bin\n", app.display())).unwrap();
        let list = dir.path().join("list");
        compose(dir.path(), &system, &list).unwrap();
        assert_eq!(std::fs::read_to_string(&list).unwrap(), format!("/nix/store/a-nyx-files/bin\n{}\n", app.display()));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&list).unwrap().permissions().mode() & 0o777, 0o644);
    }
}

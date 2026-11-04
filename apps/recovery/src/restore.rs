//! Restoring files from a Nyx Backup repository (restic) on this machine or a drive:
//! found from each person's Nyx Backup destinations, or on a drive recovery mounts
//! read-only. Files come back into a new folder in the person's home, never over theirs.

use crate::accounts::Person;
use crate::paths::Paths;
use crate::run::{Log, quiet, run};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A repository recovery can open.
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub path: PathBuf,
    /// Where it came from: a person's destination name, or the drive.
    pub label: String,
    /// A drive to mount first (`UUID=…` or a device), and where the repository is on it.
    pub drive: Option<(String, PathBuf)>,
}

/// A person's local and drive destinations from Nyx Backup's configuration.
pub fn destinations(config_toml: &str, owner: &str) -> Vec<Repo> {
    let Ok(t) = config_toml.parse::<toml::Table>() else { return vec![] };
    let Some(list) = t.get("destinations").and_then(|d| d.as_array()) else { return vec![] };
    list.iter()
        .filter_map(|d| {
            let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = s("name");
            let path = PathBuf::from(s("path"));
            match s("type").as_str() {
                "local" => Some(Repo { path, label: format!("{owner}: {name}"), drive: None }),
                "mounted" => {
                    let mount = PathBuf::from(s("mount_point"));
                    let inner = path.strip_prefix(&mount).map(Path::to_path_buf).unwrap_or_default();
                    let uuid = s("uuid");
                    let source = if uuid.is_empty() { s("source") } else { format!("UUID={uuid}") };
                    Some(Repo { path, label: format!("{owner}: {name} (drive)"), drive: Some((source, inner)) })
                }
                _ => None,
            }
        })
        .collect()
}

/// Every person's Nyx Backup destinations recovery can reach without the network.
pub fn known(people: &[Person]) -> Vec<Repo> {
    people
        .iter()
        .flat_map(|p| {
            let text = std::fs::read_to_string(Path::new(&p.home).join(".config/nyx-backup/config.toml")).unwrap_or_default();
            destinations(&text, &p.login)
        })
        .collect()
}

/// A folder that is a restic repository.
pub fn is_repo(dir: &Path) -> bool {
    dir.join("config").is_file() && dir.join("keys").is_dir() && dir.join("data").is_dir()
}

/// Repositories under `root`, a few folders deep.
pub fn find(root: &Path, depth: u32) -> Vec<PathBuf> {
    if is_repo(root) {
        return vec![root.to_path_buf()];
    }
    if depth == 0 {
        return vec![];
    }
    let Ok(entries) = std::fs::read_dir(root) else { return vec![] };
    // A drive that's the system's own root: not the store or the kernel's folders.
    let skip = ["nix", "proc", "sys", "dev"];
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !skip.iter().any(|s| e.file_name() == *s))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.iter().flat_map(|d| find(d, depth - 1)).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Drive {
    pub device: String,
    pub label: String,
    pub fstype: String,
    pub size: u64,
    pub mounted: Option<String>,
}

#[derive(Deserialize)]
struct Lsblk {
    blockdevices: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    path: String,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    children: Vec<Block>,
}

/// Filesystems from `lsblk --json --bytes -o PATH,FSTYPE,LABEL,SIZE,MOUNTPOINTS`.
pub fn parse_drives(json: &str) -> Vec<Drive> {
    fn walk(b: &Block, out: &mut Vec<Drive>) {
        if let Some(fs) =
            b.fstype.as_deref().filter(|f| !matches!(*f, "swap" | "crypto_LUKS" | "LVM2_member" | "linux_raid_member"))
        {
            out.push(Drive {
                device: b.path.clone(),
                label: b.label.clone().unwrap_or_default(),
                fstype: fs.into(),
                size: b.size.unwrap_or(0),
                // The drive itself, not the store's read-only bind mount of it.
                mounted: b.mountpoints.iter().flatten().min_by_key(|m| m.len()).cloned(),
            });
        }
        for c in &b.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    if let Ok(l) = serde_json::from_str::<Lsblk>(json) {
        for b in &l.blockdevices {
            walk(b, &mut out);
        }
    }
    out
}

pub fn drives() -> Vec<Drive> {
    quiet(&["lsblk", "--json", "--bytes", "-o", "PATH,FSTYPE,LABEL,SIZE,MOUNTPOINTS"])
        .map(|j| parse_drives(&j))
        .unwrap_or_default()
}

/// Mount `source` (a device or `UUID=…`) read-only under recovery's folder, or say where
/// it's mounted already.
pub fn mount(p: &Paths, source: &str, log: &Log) -> Result<PathBuf> {
    ensure!(source.starts_with("/dev/") || source.starts_with("UUID="), "choose a drive");
    if let Ok(out) = quiet(&["findmnt", "--noheadings", "--output", "TARGET", "--source", source])
        && let Some(at) = out.lines().map(str::trim).filter(|l| !l.is_empty()).min_by_key(|l| l.len())
    {
        return Ok(PathBuf::from(at));
    }
    let name: String =
        source.rsplit(['/', '=']).next().unwrap_or("drive").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let dir = p.mounts.join(if name.is_empty() { "drive".into() } else { name });
    std::fs::create_dir_all(&dir)?;
    run(log, &["mount", "--read-only", source, &dir.display().to_string()], None)?;
    Ok(dir)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub short_id: String,
    pub time: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Snapshot {
    /// `2026-11-04 14:05`.
    pub fn when(&self) -> String {
        self.time.get(..16).unwrap_or(&self.time).replace('T', " ")
    }
}

/// Newest first, without the whole-disk captures (those are for the recovery kit).
pub fn parse_snapshots(json: &str) -> Result<Vec<Snapshot>> {
    let mut list: Vec<Snapshot> = serde_json::from_str(json).context("restic listed something unexpected")?;
    list.retain(|s| !s.tags.iter().any(|t| t == "nyx-recovery-v1"));
    list.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(list)
}

/// The password goes to restic on its input, never on its command line.
pub fn snapshots(repo: &Path, password: &str, log: &Log) -> Result<Vec<Snapshot>> {
    let r = repo.display().to_string();
    let out =
        run(log, &["restic", "--repo", &r, "--no-cache", "--no-lock", "snapshots", "--json"], Some(&format!("{password}\n")))
            .map_err(|e| {
                if format!("{e:#}").contains("wrong password") {
                    anyhow::anyhow!("That password didn't open the backup.")
                } else {
                    e
                }
            })?;
    parse_snapshots(&out)
}

/// A new folder in `person`'s home for what comes back.
pub fn target_for(person: &Person, snap: &Snapshot) -> PathBuf {
    Path::new(&person.home).join(format!("Restored {} {}", snap.when().replace(':', "."), snap.short_id))
}

/// Restore `snap` (or only `only`, a path inside it) into a new folder in `person`'s home,
/// owned by them.
pub fn restore(repo: &Path, password: &str, snap: &Snapshot, only: Option<&str>, person: &Person, log: &Log) -> Result<String> {
    let target = target_for(person, snap);
    if target.exists() {
        bail!("{} is there already; move it first", target.display());
    }
    let (r, t) = (repo.display().to_string(), target.display().to_string());
    // Drives are mounted read-only, where restic can't leave a lock.
    let mut args = vec!["restic", "--repo", &r, "--no-cache", "--no-lock", "restore", &snap.id, "--target", &t];
    if let Some(o) = only {
        ensure!(o.starts_with('/') && !o.contains(".."), "choose a path in the backup, like /home/{}/Documents", person.login);
        args.extend(["--include", o]);
    }
    run(log, &args, Some(&format!("{password}\n")))?;
    // `login:` is the person and their own group.
    run(log, &["chown", "-R", &format!("{}:", person.login), &t], None)?;
    Ok(format!("Restored into {t}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_a_recovery_can_reach() {
        let cfg = r#"
[[destinations]]
id = "a"
name = "Home disk"
type = "local"
path = "/home/ken/Backups"

[[destinations]]
id = "b"
name = "Black drive"
type = "mounted"
path = "/run/media/ken/Black/nyx"
mount_point = "/run/media/ken/Black"
source = "/dev/sdb1"
filesystem = "ext4"
uuid = "1234-abcd"

[[destinations]]
id = "c"
name = "NAS"
type = "sftp"
host = "nas"
path = "/backups"
"#;
        let d = destinations(cfg, "ken");
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Repo { path: "/home/ken/Backups".into(), label: "ken: Home disk".into(), drive: None });
        assert_eq!(d[1].drive, Some(("UUID=1234-abcd".into(), PathBuf::from("nyx"))));
        assert!(destinations("not toml [", "ken").is_empty());
    }

    #[test]
    fn repositories_are_found_a_few_folders_down() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("backups/laptop");
        for d in ["keys", "data"] {
            std::fs::create_dir_all(repo.join(d)).unwrap();
        }
        std::fs::write(repo.join("config"), "x").unwrap();
        std::fs::create_dir_all(t.path().join("photos/2026")).unwrap();
        assert_eq!(find(t.path(), 3), vec![repo.clone()]);
        assert!(find(t.path(), 1).is_empty());
        assert!(is_repo(&repo));
    }

    #[test]
    fn drives_from_lsblk() {
        let j = r#"{"blockdevices":[
          {"path":"/dev/sda","fstype":null,"label":null,"size":100,"mountpoints":[null],"children":[
            {"path":"/dev/sda1","fstype":"vfat","label":"ESP","size":10,"mountpoints":["/boot"]},
            {"path":"/dev/sda2","fstype":"ext4","label":"nixos","size":90,"mountpoints":["/nix/store","/"]},
            {"path":"/dev/sda3","fstype":"swap","label":null,"size":5,"mountpoints":["[SWAP]"]}]},
          {"path":"/dev/sdb1","fstype":"exfat","label":"Black","size":500,"mountpoints":[null]}]}"#;
        let d = parse_drives(j);
        assert_eq!(d.iter().map(|x| x.device.as_str()).collect::<Vec<_>>(), vec!["/dev/sda1", "/dev/sda2", "/dev/sdb1"]);
        assert_eq!(d[1].mounted.as_deref(), Some("/"));
        assert_eq!(d[2].mounted, None);
    }

    #[test]
    fn snapshots_newest_first_without_disk_captures() {
        let j = r#"[
          {"id":"aaaa1111","short_id":"aaaa1111","time":"2026-11-01T10:00:00.1-06:00","hostname":"nyx","paths":["/home/ken"],"tags":["nyx-backup"]},
          {"id":"bbbb2222","short_id":"bbbb2222","time":"2026-11-03T09:30:00.1-06:00","hostname":"nyx","paths":["/home/ken"]},
          {"id":"cccc3333","short_id":"cccc3333","time":"2026-11-04T09:30:00.1-06:00","paths":["/dev/sda"],"tags":["nyx-recovery-v1"]}]"#;
        let s = parse_snapshots(j).unwrap();
        assert_eq!(s.iter().map(|x| x.short_id.as_str()).collect::<Vec<_>>(), vec!["bbbb2222", "aaaa1111"]);
        assert_eq!(s[0].when(), "2026-11-03 09:30");
        let ken = Person { login: "ken".into(), name: "Ken".into(), uid: 1000, home: "/home/ken".into(), locked: false };
        assert_eq!(target_for(&ken, &s[0]), PathBuf::from("/home/ken/Restored 2026-11-03 09.30 bbbb2222"));
    }
}

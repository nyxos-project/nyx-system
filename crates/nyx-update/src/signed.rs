//! Release signatures. NyxOS's stable channel moves only to a revision that carries a
//! `known-good/<n>` tag signed by a release key: the keys in [`SIGNERS`] (ssh-keygen's
//! allowed-signers format), checked with `git verify-tag`. The testing channel isn't
//! signed.

use anyhow::{Context, Result, bail, ensure};
use std::path::Path;
use std::process::Command;

/// The release keys NyxOS ships.
pub const SIGNERS: &str = "/etc/nyxos/release-signers";

/// The channel the NyxOS input follows, as a flake.lock pins it.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub owner: String,
    pub repo: String,
    /// The branch it follows (`stable`).
    pub branch: String,
    pub rev: String,
}

impl Channel {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }

    /// Only stable is signed, and only where the system has release keys.
    pub fn needs_signature(&self, signers: &Path) -> bool {
        self.branch == "stable" && signers.is_file()
    }
}

fn plain(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with(['-', '.'])
}

/// The `nyxos` input of a flake.lock, when it follows a GitHub branch.
pub fn channel_in_lock(lock: &str) -> Option<Channel> {
    let v: serde_json::Value = serde_json::from_str(lock).ok()?;
    let root = v["root"].as_str()?;
    let node = v["nodes"][root]["inputs"]["nyxos"].as_str()?;
    let node = &v["nodes"][node];
    let (locked, original) = (&node["locked"], &node["original"]);
    if locked["type"] != "github" {
        return None;
    }
    let text = |x: &serde_json::Value| x.as_str().map(str::to_string);
    let channel = Channel {
        owner: text(&locked["owner"])?,
        repo: text(&locked["repo"])?,
        branch: text(&original["ref"]).unwrap_or_default(),
        rev: text(&locked["rev"])?,
    };
    (plain(&channel.owner) && plain(&channel.repo) && channel.rev.chars().all(|c| c.is_ascii_hexdigit())).then_some(channel)
}

/// `known-good/<n>` tags whose commit is `rev`, newest first, from `git ls-remote`.
pub fn tags_at(ls_remote: &str, rev: &str) -> Vec<String> {
    let mut tags: Vec<(u64, String)> = ls_remote
        .lines()
        .filter_map(|l| {
            let (sha, name) = l.split_once('\t')?;
            let tag = name.strip_prefix("refs/tags/")?.strip_suffix("^{}")?;
            let n: u64 = tag.strip_prefix("known-good/")?.parse().ok()?;
            (sha == rev).then(|| (n, tag.to_string()))
        })
        .collect();
    tags.sort_by(|a, b| b.0.cmp(&a.0));
    tags.into_iter().map(|(_, t)| t).collect()
}

fn git(args: &[&str]) -> Result<std::process::Output> {
    Command::new("git").args(args).env("GIT_TERMINAL_PROMPT", "0").output().context("git isn't installed")
}

/// The signed tag that vouches for `rev` of `url`, or why there's none.
pub fn verify(url: &str, rev: &str, signers: &Path) -> Result<String> {
    let short = &rev[..rev.len().min(7)];
    let out = git(&["ls-remote", "--tags", url, "refs/tags/known-good/*"])?;
    ensure!(out.status.success(), "couldn't list NyxOS's releases: {}", String::from_utf8_lossy(&out.stderr).trim());
    let tags = tags_at(&String::from_utf8_lossy(&out.stdout), rev);
    ensure!(!tags.is_empty(), "NyxOS {short} isn't a release (it has no known-good tag), so it isn't installed");
    let dir = tempfile::tempdir()?;
    let at = dir.path().to_str().context("temporary folder")?;
    ensure!(git(&["init", "-q", "--bare", at])?.status.success(), "git init failed");
    let signers = signers.to_str().context("signers path")?;
    for tag in &tags {
        let spec = format!("+refs/tags/{tag}:refs/tags/{tag}");
        if !git(&["-C", at, "fetch", "-q", "--depth=1", "--no-tags", url, &spec])?.status.success() {
            continue;
        }
        let good = git(&["-C", at, "-c", "gpg.format=ssh", "-c", &format!("gpg.ssh.allowedSignersFile={signers}"), "verify-tag", tag])?
            .status
            .success();
        let commit = git(&["-C", at, "rev-parse", &format!("{tag}^{{commit}}")])?;
        if good && String::from_utf8_lossy(&commit.stdout).trim() == rev {
            return Ok(tag.clone());
        }
    }
    bail!("NyxOS {short} isn't signed by a NyxOS release key, so it isn't installed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nyxos_input_of_a_lock() {
        let lock = r#"{"root":"root","version":7,"nodes":{
            "root":{"inputs":{"nyxos":"nyxos_2"}},
            "nyxos_2":{"locked":{"type":"github","owner":"nyxos-project","repo":"nyxos","rev":"3a680dfc51c01d72d1e3840152bc67aa93b6e861"},
                       "original":{"type":"github","owner":"nyxos-project","repo":"nyxos","ref":"stable"}}}}"#;
        let c = channel_in_lock(lock).unwrap();
        assert_eq!((c.branch.as_str(), c.url()), ("stable", "https://github.com/nyxos-project/nyxos".to_string()));
        assert_eq!(c.rev, "3a680dfc51c01d72d1e3840152bc67aa93b6e861");
        assert!(channel_in_lock(r#"{"root":"root","nodes":{"root":{"inputs":{}}}}"#).is_none());
        let path = lock.replace(r#""type":"github","owner""#, r#""type":"path","owner""#);
        assert!(channel_in_lock(&path).is_none());
    }

    #[test]
    fn only_tags_at_the_revision_newest_first() {
        let ls = "1111\trefs/tags/known-good/22\naaaa\trefs/tags/known-good/22^{}\n\
                  2222\trefs/tags/known-good/23\nbbbb\trefs/tags/known-good/23^{}\n\
                  3333\trefs/tags/known-good/24\nbbbb\trefs/tags/known-good/24^{}\n\
                  4444\trefs/tags/v0.1.0\nbbbb\trefs/tags/v0.1.0^{}\n";
        assert_eq!(tags_at(ls, "bbbb"), vec!["known-good/24", "known-good/23"]);
        assert!(tags_at(ls, "cccc").is_empty());
    }

    /// A tag signed with a throwaway key verifies against its allowed-signers line, and
    /// not against another key's.
    #[test]
    fn signed_tags_verify_against_the_signers() {
        if Command::new("ssh-keygen").arg("-?").output().is_err() || git(&["--version"]).is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = |p: &str| dir.path().join(p).display().to_string();
        let keygen = |name: &str| {
            assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f", &d(name)]).status().unwrap().success());
            std::fs::read_to_string(d(&format!("{name}.pub"))).unwrap()
        };
        let (good, other) = (keygen("good"), keygen("other"));
        let repo = d("repo");
        let g = |args: &[&str]| {
            let mut all = vec!["-C", repo.as_str(), "-c", "user.name=t", "-c", "user.email=t@t"];
            all.extend_from_slice(args);
            assert!(git(&all).unwrap().status.success(), "{args:?}");
        };
        assert!(git(&["init", "-q", &repo]).unwrap().status.success());
        g(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let key = d("good");
        g(&["-c", "gpg.format=ssh", "-c", &format!("user.signingkey={key}"), "tag", "-s", "known-good/1", "-m", "one"]);
        g(&["tag", "-a", "known-good/2", "-m", "unsigned"]);
        let rev = String::from_utf8(git(&["-C", &repo, "rev-parse", "HEAD"]).unwrap().stdout).unwrap().trim().to_string();
        let signers = dir.path().join("signers");
        std::fs::write(&signers, format!("nyxos-release namespaces=\"git\" {good}")).unwrap();
        let url = format!("file://{repo}");
        assert_eq!(verify(&url, &rev, &signers).unwrap(), "known-good/1");
        std::fs::write(&signers, format!("nyxos-release namespaces=\"git\" {other}")).unwrap();
        assert!(verify(&url, &rev, &signers).unwrap_err().to_string().contains("isn't signed"));
    }
}

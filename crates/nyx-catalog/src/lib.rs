//! The NyxOS software catalog (`catalog.toml`), shared by Nyx Welcome, Nyx Store
//! and `nyx-system catalog`: sections and categories, the items in three tiers (base,
//! welcome, store) and the presets.
//!
//! A selection becomes a [`Plan`]: the System Manager [`Change`]s that install it for
//! everyone (in one rebuild), the Flathub apps installed for the user, and what the
//! user has to agree to first (unfree licenses, services, groups).

use anyhow::{Context, Result, bail, ensure};
use nyx_system_proto::{Change, GROUPS, SERVICES, SystemSettings, valid_nyx_app, valid_package};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Where NyxOS installs the catalog it was built with.
pub const PATH: &str = "/run/current-system/sw/share/nyx/catalog.toml";

/// The Nyx apps the running system has (`nyx.<name>.enable`), one per line.
pub const NYX_APPS: &str = "/etc/nyx/apps";

const BUILTIN: &str = include_str!("../catalog.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Preinstalled.
    Base,
    /// Offered in Welcome (and the Store).
    Welcome,
    /// Only in the Store.
    Store,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub id: String,
    pub name: String,
    pub section: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub category: String,
    pub tier: Tier,
    /// nixpkgs attributes, installed for everyone.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Nyx apps (`nyx.<name>.enable`).
    #[serde(default)]
    pub nyx: Vec<String>,
    /// A Flathub app, installed for the user.
    #[serde(default)]
    pub flatpak: Option<String>,
    /// A flake installed into the user's Nix profile (`nix profile add`), for tools
    /// nixpkgs doesn't have.
    #[serde(default)]
    pub profile: Option<String>,
    /// System services it turns on.
    #[serde(default)]
    pub services: Vec<String>,
    /// Groups the user joins.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Its license isn't free: the user accepts it.
    #[serde(default)]
    pub unfree: bool,
    /// A command that shows it's installed, however it got there.
    #[serde(default)]
    pub bin: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(rename = "section")]
    pub sections: Vec<Section>,
    #[serde(rename = "category")]
    pub categories: Vec<Category>,
    #[serde(rename = "item")]
    pub items: Vec<Item>,
    #[serde(rename = "preset", default)]
    pub presets: Vec<Preset>,
}

/// Something the user agrees to before a plan is applied.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Consent {
    /// An unfree license, for the item named.
    License { item: String },
    /// A system service turned on, with what it is.
    Service { service: String, what: String },
    /// Joining a group, with what it allows.
    Group { group: String, what: String },
}

impl Consent {
    pub fn describe(&self) -> String {
        match self {
            Consent::License { item } => format!("{item} has a license that isn't free; installing it accepts that license"),
            Consent::Service { what, .. } => format!("Turns on a system service: {what}"),
            Consent::Group { group, what } => format!("Adds you to the {group} group: {what}"),
        }
    }
}

/// What installing a selection takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// For the System Manager, applied together in one rebuild.
    pub changes: Vec<Change>,
    /// Flathub apps for the user.
    pub flatpaks: Vec<String>,
    /// Flakes for the user's Nix profile.
    pub profiles: Vec<String>,
    pub consent: Vec<Consent>,
}

impl Plan {
    /// Whether the system has to be rebuilt.
    pub fn rebuilds(&self) -> bool {
        !self.changes.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.flatpaks.is_empty() && self.profiles.is_empty()
    }
}

impl Catalog {
    /// The catalog this crate was built with.
    pub fn builtin() -> Result<Catalog> {
        Catalog::parse(BUILTIN)
    }

    /// The installed system's catalog, or the built-in one.
    pub fn load() -> Result<Catalog> {
        let path = Path::new(PATH);
        if path.exists() {
            let text = std::fs::read_to_string(path).with_context(|| PATH.to_string())?;
            return Catalog::parse(&text).with_context(|| PATH.to_string());
        }
        Catalog::builtin()
    }

    pub fn parse(text: &str) -> Result<Catalog> {
        let c: Catalog = toml::from_str(text)?;
        c.check()?;
        Ok(c)
    }

    fn check(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        for s in &self.sections {
            ensure!(ids.insert(format!("s:{}", s.id)), "section {} is listed twice", s.id);
        }
        for c in &self.categories {
            ensure!(ids.insert(format!("c:{}", c.id)), "category {} is listed twice", c.id);
            ensure!(self.sections.iter().any(|s| s.id == c.section), "category {} is in no section ({})", c.id, c.section);
        }
        for i in &self.items {
            ensure!(ids.insert(format!("i:{}", i.id)), "item {} is listed twice", i.id);
            ensure!(self.categories.iter().any(|c| c.id == i.category), "item {} is in no category ({})", i.id, i.category);
            ensure!(
                !i.packages.is_empty()
                    || !i.nyx.is_empty()
                    || i.flatpak.is_some()
                    || i.profile.is_some()
                    || !i.services.is_empty(),
                "item {} installs nothing",
                i.id
            );
            if let Some(f) = &i.profile {
                ensure!(valid_flake_ref(f), "item {}: not a flake reference Nyx installs from: {f}", i.id);
                ensure!(i.bin.is_some(), "item {} comes from a flake but names no command", i.id);
            }
            for p in &i.packages {
                ensure!(valid_package(p), "item {}: not a nixpkgs attribute: {p}", i.id);
            }
            for n in &i.nyx {
                ensure!(valid_nyx_app(n), "item {}: not a Nyx app name: {n}", i.id);
            }
            if let Some(f) = &i.flatpak {
                ensure!(valid_flatpak(f), "item {}: not a Flatpak app ID: {f}", i.id);
            }
            for s in &i.services {
                ensure!(SERVICES.iter().any(|(n, _)| n == s), "item {}: not a service the System Manager has: {s}", i.id);
            }
            for g in &i.groups {
                ensure!(GROUPS.iter().any(|(n, _)| n == g), "item {}: not a group the System Manager has: {g}", i.id);
            }
            ensure!(
                !i.unfree || !i.packages.is_empty() || i.flatpak.is_some() || i.profile.is_some(),
                "item {} is unfree but installs no package",
                i.id
            );
            ensure!(i.tier != Tier::Base || i.flatpak.is_none(), "base item {} can't come from Flathub", i.id);
            ensure!(i.tier != Tier::Base || i.profile.is_none(), "base item {} can't come from a flake", i.id);
        }
        for p in &self.presets {
            ensure!(ids.insert(format!("p:{}", p.id)), "preset {} is listed twice", p.id);
            for i in &p.items {
                ensure!(self.item(i).is_some(), "preset {} has an item that isn't in the catalog: {i}", p.id);
            }
        }
        Ok(())
    }

    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn section(&self, id: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }

    pub fn category(&self, id: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.id == id)
    }

    /// The section an item is in.
    pub fn section_of(&self, item: &Item) -> Option<&Section> {
        self.category(&item.category).and_then(|c| self.section(&c.section))
    }

    /// A section's items of the given tiers, in catalog order.
    pub fn items_in<'a>(&'a self, section: &'a str, tiers: &'a [Tier]) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |i| tiers.contains(&i.tier) && self.category(&i.category).is_some_and(|c| c.section == section))
    }

    /// The items of the presets given, together.
    pub fn presets(&self, ids: &[&str]) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for id in ids {
            let Some(p) = self.presets.iter().find(|p| p.id == *id) else { bail!("no preset {id}") };
            out.extend(p.items.iter().cloned());
        }
        Ok(out)
    }

    /// What installing `ids` for `user` takes. Items already installed are left out.
    pub fn plan(&self, ids: &BTreeSet<String>, user: &str, have: &Installed) -> Result<Plan> {
        let mut packages: Vec<String> = Vec::new();
        let mut unfree: Vec<String> = Vec::new();
        let mut services = BTreeSet::new();
        let mut nyx = BTreeSet::new();
        let mut groups = BTreeSet::new();
        let mut plan = Plan::default();
        let mut consent = BTreeSet::new();
        for id in ids {
            let Some(i) = self.item(id) else { bail!("no item {id} in the catalog") };
            if have.has(i) {
                continue;
            }
            for p in &i.packages {
                if !packages.contains(p) && !have.settings.packages.contains(p) {
                    packages.push(p.clone());
                    if i.unfree {
                        unfree.push(p.clone());
                    }
                }
            }
            if i.unfree {
                consent.insert(Consent::License { item: i.name.clone() });
            }
            nyx.extend(i.nyx.iter().filter(|n| !have.nyx_apps.contains(*n)).cloned());
            if let Some(f) = &i.flatpak
                && !have.flatpaks.contains(f)
                && !plan.flatpaks.contains(f)
            {
                plan.flatpaks.push(f.clone());
            }
            if let Some(f) = &i.profile
                && !plan.profiles.contains(f)
            {
                plan.profiles.push(f.clone());
            }
            for s in &i.services {
                if have.settings.services.get(s) != Some(&true) && services.insert(s.clone()) {
                    let what = SERVICES.iter().find(|(n, _)| n == s).map(|(_, w)| w.to_string()).unwrap_or_default();
                    consent.insert(Consent::Service { service: s.clone(), what });
                }
            }
            for g in &i.groups {
                let member = have.settings.groups.get(g).is_some_and(|u| u.contains(user));
                if !member && groups.insert(g.clone()) {
                    let what = GROUPS.iter().find(|(n, _)| n == g).map(|(_, w)| w.to_string()).unwrap_or_default();
                    consent.insert(Consent::Group { group: g.clone(), what });
                }
            }
        }
        for chunk in packages.chunks(100) {
            let unfree = unfree.iter().filter(|u| chunk.contains(u)).cloned().collect();
            plan.changes.push(Change::AddPackages { packages: chunk.to_vec(), unfree });
        }
        for name in nyx {
            plan.changes.push(Change::NyxApp { name, enabled: Some(true) });
        }
        for name in services {
            plan.changes.push(Change::Service { name, enabled: Some(true) });
        }
        for group in groups {
            plan.changes.push(Change::GroupMember { group, user: user.to_string(), member: true });
        }
        plan.consent = consent.into_iter().collect();
        Ok(plan)
    }

    /// What removing `ids` takes: their packages, Nyx apps and Flathub apps. Services and
    /// groups stay, since something else may use them.
    pub fn removal(&self, ids: &BTreeSet<String>, have: &Installed) -> Result<Plan> {
        let mut packages = Vec::new();
        let mut plan = Plan::default();
        let mut nyx = BTreeSet::new();
        for id in ids {
            let Some(i) = self.item(id) else { bail!("no item {id} in the catalog") };
            ensure!(i.tier != Tier::Base || !i.nyx.is_empty(), "{} is part of NyxOS", i.name);
            packages.extend(i.packages.iter().filter(|p| have.settings.packages.contains(*p)).cloned());
            nyx.extend(i.nyx.iter().filter(|n| have.nyx_apps.contains(*n)).cloned());
            if let Some(f) = &i.flatpak
                && have.flatpaks.contains(f)
            {
                plan.flatpaks.push(f.clone());
            }
            if let Some(f) = &i.profile
                && have.profiles.contains(f)
            {
                plan.profiles.push(f.clone());
            }
        }
        packages.sort();
        packages.dedup();
        for chunk in packages.chunks(100) {
            plan.changes.push(Change::RemovePackages { packages: chunk.to_vec() });
        }
        for name in nyx {
            plan.changes.push(Change::NyxApp { name, enabled: Some(false) });
        }
        Ok(plan)
    }
}

/// What the system and the user have now.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    /// The System Manager's settings.
    pub settings: SystemSettings,
    /// Nyx apps the running system has ([`NYX_APPS`]).
    pub nyx_apps: BTreeSet<String>,
    /// Flathub apps, the user's and the system's.
    pub flatpaks: BTreeSet<String>,
    /// Flakes in the user's Nix profile, as they were added.
    pub profiles: BTreeSet<String>,
    /// Commands on the system's and the user's paths.
    pub bins: BTreeSet<String>,
}

impl Installed {
    /// Look at the running system, with the System Manager's `settings`.
    pub fn probe(settings: SystemSettings) -> Installed {
        let nyx_apps = std::fs::read_to_string(NYX_APPS)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let flatpaks = std::process::Command::new("flatpak")
            .args(["list", "--app", "--columns=application"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        let mut bins = BTreeSet::new();
        let user = std::env::var("USER").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        let state = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| format!("{home}/.local/state"));
        for dir in [
            "/run/current-system/sw/bin".to_string(),
            format!("/etc/profiles/per-user/{user}/bin"),
            format!("{home}/.nix-profile/bin"),
            format!("{state}/nix/profile/bin"),
        ] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                bins.extend(entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()));
            }
        }
        let profiles = profile_elements().into_values().collect();
        Installed { settings, nyx_apps, flatpaks, profiles, bins }
    }

    /// Whether `item` is installed.
    pub fn has(&self, item: &Item) -> bool {
        if !item.nyx.is_empty() {
            return item.nyx.iter().all(|n| self.nyx_apps.contains(n));
        }
        if let Some(f) = &item.flatpak {
            return self.flatpaks.contains(f);
        }
        if item.profile.as_ref().is_some_and(|f| self.profiles.contains(f)) {
            return true;
        }
        if item.bin.as_ref().is_some_and(|b| self.bins.contains(b)) {
            return true;
        }
        if !item.packages.is_empty() {
            return item.packages.iter().all(|p| self.settings.packages.contains(p));
        }
        !item.services.is_empty() && item.services.iter().all(|s| self.settings.services.get(s) == Some(&true))
    }
}

/// Install Flathub apps for the user (adding the user's Flathub remote first), or
/// uninstall them. `line` gets flatpak's output as it goes.
pub fn flatpak(apps: &[String], install: bool, mut line: impl FnMut(&str)) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    if apps.is_empty() {
        return Ok(());
    }
    for a in apps {
        ensure!(valid_flatpak(a), "not a Flatpak app ID: {a}");
    }
    let mut steps: Vec<Vec<String>> = Vec::new();
    if install {
        steps.push(
            ["remote-add", "--user", "--if-not-exists", "flathub", "https://dl.flathub.org/repo/flathub.flatpakrepo"]
                .map(String::from)
                .to_vec(),
        );
        steps.push(
            [
                &["install", "--user", "--noninteractive", "-y", "flathub"][..],
                &apps.iter().map(String::as_str).collect::<Vec<_>>(),
            ]
            .concat()
            .into_iter()
            .map(String::from)
            .collect(),
        );
    } else {
        steps.push(
            [&["uninstall", "--noninteractive", "-y"][..], &apps.iter().map(String::as_str).collect::<Vec<_>>()]
                .concat()
                .into_iter()
                .map(String::from)
                .collect(),
        );
    }
    for args in steps {
        let mut child =
            Command::new("flatpak").args(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("flatpak")?;
        let err = child
            .stderr
            .take()
            .map(|e| std::thread::spawn(move || BufReader::new(e).lines().map_while(Result::ok).collect::<Vec<_>>()));
        if let Some(out) = child.stdout.take() {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                line(&l);
            }
        }
        let errors = err.and_then(|h| h.join().ok()).unwrap_or_default();
        for l in &errors {
            line(l);
        }
        let status = child.wait()?;
        ensure!(status.success(), "flatpak {} failed: {}", args[0], errors.last().cloned().unwrap_or_default());
    }
    Ok(())
}

/// The user's Nix profile: each element's name and the flake it was added from.
pub fn profile_elements() -> BTreeMap<String, String> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        elements: BTreeMap<String, Element>,
    }
    #[derive(Deserialize)]
    struct Element {
        #[serde(rename = "originalUrl")]
        original_url: Option<String>,
    }
    let Ok(out) = std::process::Command::new("nix").args(["profile", "list", "--json"]).output() else {
        return BTreeMap::new();
    };
    let Ok(list) = serde_json::from_slice::<List>(&out.stdout) else { return BTreeMap::new() };
    list.elements.into_iter().filter_map(|(name, e)| Some((name, e.original_url?))).collect()
}

/// Add flakes to the user's Nix profile, or remove them. `line` gets nix's output.
pub fn nix_profile(refs: &[String], install: bool, mut line: impl FnMut(&str)) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    for r in refs {
        ensure!(valid_flake_ref(r), "not a flake reference Nyx installs from: {r}");
    }
    let mut runs: Vec<Vec<String>> = Vec::new();
    if install {
        runs.extend(refs.iter().map(|r| vec!["add".to_string(), r.clone()]));
    } else {
        let names: Vec<String> =
            profile_elements().into_iter().filter(|(_, url)| refs.contains(url)).map(|(name, _)| name).collect();
        if !names.is_empty() {
            runs.push([vec!["remove".to_string()], names].concat());
        }
    }
    for args in runs {
        let mut child = Command::new("nix")
            .arg("profile")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("nix profile")?;
        let mut last = String::new();
        if let Some(err) = child.stderr.take() {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                if l.trim_start().starts_with("error:") {
                    last = l.trim().to_string();
                }
                line(&l);
            }
        }
        ensure!(child.wait()?.success(), "nix profile {} failed: {last}", args[0]);
    }
    Ok(())
}

/// `github:owner/repo`, optionally with a ref (`/v1.2`) and an output (`#name`).
pub fn valid_flake_ref(r: &str) -> bool {
    let Some(rest) = ["github:", "gitlab:", "sourcehut:"].iter().find_map(|p| r.strip_prefix(p)) else { return false };
    let (path, output) = rest.split_once('#').unwrap_or((rest, ""));
    let parts: Vec<&str> = path.split('/').collect();
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-~".contains(&b));
    r.len() <= 200
        && (2..=3).contains(&parts.len())
        && parts.iter().all(|p| ok(p) && !p.starts_with('.'))
        && (output.is_empty() || ok(output))
}

/// `com.valvesoftware.Steam`.
pub fn valid_flatpak(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 3
        && id.len() <= 255
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && !p.starts_with(|c: char| c.is_ascii_digit())
        })
}

/// The packages per tier, for checks against a NixOS configuration.
pub fn packages_by_tier(c: &Catalog) -> BTreeMap<Tier, BTreeSet<String>> {
    let mut out: BTreeMap<Tier, BTreeSet<String>> = BTreeMap::new();
    for i in &c.items {
        out.entry(i.tier).or_default().extend(i.packages.iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::builtin().expect("the built-in catalog is valid")
    }

    #[test]
    fn builtin_is_valid_and_has_every_section() {
        let c = catalog();
        for s in ["nyx-apps", "desktop", "cli", "toolchains", "infrastructure", "agents"] {
            assert!(c.items_in(s, &[Tier::Welcome]).count() > 0, "{s} offers nothing in Welcome");
        }
        for p in ["everyday", "developer", "ai-developer", "homelab"] {
            assert!(c.presets(&[p]).is_ok());
        }
    }

    #[test]
    fn a_plan_is_one_rebuild_with_consent() {
        let c = catalog();
        let pick: BTreeSet<String> = ["docker", "vscode", "nyx-mail", "steam", "firefox"].map(String::from).into();
        let plan = c.plan(&pick, "ken", &Installed::default()).unwrap();
        let Change::AddPackages { packages, unfree } = &plan.changes[0] else { panic!("{:?}", plan.changes) };
        assert!(packages.contains(&"docker-compose".into()) && packages.contains(&"firefox".into()));
        assert_eq!(unfree, &vec!["vscode".to_string()]);
        assert!(plan.changes.contains(&Change::NyxApp { name: "mail".into(), enabled: Some(true) }));
        assert!(plan.changes.contains(&Change::Service { name: "docker".into(), enabled: Some(true) }));
        assert!(plan.changes.contains(&Change::GroupMember { group: "docker".into(), user: "ken".into(), member: true }));
        assert_eq!(plan.flatpaks, vec!["com.valvesoftware.Steam".to_string()]);
        assert!(plan.consent.iter().any(|c| matches!(c, Consent::Group { group, .. } if group == "docker")));
        assert_eq!(plan.consent.iter().filter(|c| matches!(c, Consent::License { .. })).count(), 2);
        for ch in &plan.changes {
            ch.validate().unwrap();
        }
    }

    #[test]
    fn installed_items_are_left_out() {
        let c = catalog();
        let mut have = Installed::default();
        have.bins.insert("firefox".into());
        have.nyx_apps.insert("mail".into());
        have.settings.services.insert("docker".into(), true);
        have.settings.groups.entry("docker".into()).or_default().insert("ken".into());
        have.settings.packages.insert("docker-compose".into());
        let pick: BTreeSet<String> = ["firefox", "nyx-mail", "docker"].map(String::from).into();
        assert!(c.plan(&pick, "ken", &have).unwrap().is_empty());
        let removal = c.removal(&pick, &have).unwrap();
        assert_eq!(
            removal.changes,
            vec![
                Change::RemovePackages { packages: vec!["docker-compose".into()] },
                Change::NyxApp { name: "mail".into(), enabled: Some(false) },
            ]
        );
    }

    #[test]
    fn flake_items_go_to_the_users_profile() {
        let c = catalog();
        let pick: BTreeSet<String> = ["oh-my-pi"].map(String::from).into();
        let plan = c.plan(&pick, "ken", &Installed::default()).unwrap();
        assert!(plan.changes.is_empty() && !plan.rebuilds());
        assert_eq!(plan.profiles, vec!["github:can1357/oh-my-pi".to_string()]);
        let mut have = Installed::default();
        have.profiles.insert("github:can1357/oh-my-pi".into());
        assert!(c.plan(&pick, "ken", &have).unwrap().is_empty());
        assert_eq!(c.removal(&pick, &have).unwrap().profiles, plan.profiles);
        for good in ["github:can1357/oh-my-pi", "github:a/b/v1.2", "gitlab:a/b#pkg"] {
            assert!(valid_flake_ref(good), "{good}");
        }
        for bad in ["github:a", "path:/etc", "github:a/b?dir=x", "github:../b", "https://x/y", "github:a/b#x y"] {
            assert!(!valid_flake_ref(bad), "{bad}");
        }
    }

    #[test]
    fn bad_catalogs_are_refused() {
        let base = "[[section]]\nid='s'\nname='S'\nsummary=''\n[[category]]\nid='c'\nname='C'\nsection='s'\n";
        for item in [
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\n",
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\npackages=['a;b']\n",
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\nservices=['sshd']\n",
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\npackages=['a']\ngroups=['wheel']\n",
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='nope'\ntier='welcome'\npackages=['a']\n",
            "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\nflatpak='steam'\n",
        ] {
            assert!(Catalog::parse(&format!("{base}{item}")).is_err(), "{item}");
        }
        assert!(
            Catalog::parse(&format!(
                "{base}[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\ntier='welcome'\npackages=['a']\n"
            ))
            .is_ok()
        );
    }
}

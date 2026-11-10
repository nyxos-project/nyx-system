//! The NyxOS software catalog (`catalog.toml`), shared by Nyx Welcome, Nyx Store
//! and `nyx-system catalog`: sections and categories, the items in three tiers (base,
//! welcome, store) and the presets.
//!
//! Each item has a [`Scope`]. Most software is the user's own: it goes into their Nix
//! profile from the system's channel (`github:nyxos-project/nyxos/stable#firefox`), or
//! Flathub, with no password and no rebuild. Only what the system has to provide
//! (services, groups, drivers, Nyx components built into the desktop) is installed for
//! everyone.
//!
//! A selection becomes a [`Plan`]: the System Manager [`Change`]s for the system's part
//! (in one rebuild), the Flathub apps and profile packages for the user, and what the
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

/// The system flake, whose `nyxos` input is the channel people's profiles install from.
pub const SYSTEM_FLAKE_LOCK: &str = "/etc/nyxos/flake/flake.lock";

/// The channel when the system's flake doesn't say (a machine that isn't NyxOS).
pub const DEFAULT_CHANNEL: &str = "github:nyxos-project/nyxos/stable";

const BUILTIN: &str = include_str!("../catalog.toml");

/// Who an item is installed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The person installing it: their Nix profile or Flathub.
    User,
    /// Everyone, through the System Manager (a rebuild, the administrator's password).
    System,
}

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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub category: String,
    pub tier: Tier,
    /// Who it's installed for, when it isn't what [`Item::scope`] works out.
    #[serde(default, rename = "scope")]
    pub scope_override: Option<Scope>,
    /// More names it answers to (`nyx install NAME`, `nyx update NAME`).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// nixpkgs attributes: the user's profile, or the system's packages.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Nyx apps: `nyx-<name>` in the user's profile, or `nyx.<name>.enable`.
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

impl Item {
    /// Who it's installed for: the system when NyxOS ships it (base), when it turns on a
    /// service or adds the user to a group, or when the catalog says so; otherwise the
    /// person installing it.
    pub fn scope(&self) -> Scope {
        if let Some(s) = self.scope_override {
            return s;
        }
        if self.tier == Tier::Base || !self.services.is_empty() || !self.groups.is_empty() { Scope::System } else { Scope::User }
    }

    /// The attributes a profile installs it as: its packages and its Nyx apps' packages.
    pub fn profile_attrs(&self) -> Vec<String> {
        self.packages.iter().cloned().chain(self.nyx.iter().map(|n| format!("nyx-{n}"))).collect()
    }

    /// Every name it answers to, lowercase: its id, name, aliases, command, packages and
    /// Nyx apps.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = [self.id.clone(), self.name.clone()]
            .into_iter()
            .chain(self.aliases.iter().cloned())
            .chain(self.bin.iter().cloned())
            .chain(self.packages.iter().map(|p| p.rsplit('.').next().unwrap_or(p).to_string()))
            .chain(self.nyx.iter().cloned())
            .chain(self.nyx.iter().map(|n| format!("nyx-{n}")))
            .map(|n| n.to_lowercase())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn answers_to(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.names().contains(&n)
    }
}

/// What a name stands for in the catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum Found<'a> {
    One(&'a Item),
    Many(Vec<&'a Item>),
    None,
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
            if i.scope_override == Some(Scope::User) {
                ensure!(i.tier != Tier::Base, "base item {} is installed for everyone, not per user", i.id);
                ensure!(
                    i.services.is_empty() && i.groups.is_empty(),
                    "item {} turns on services or groups, so it's installed for everyone, not per user",
                    i.id
                );
            }
            ensure!(
                i.scope() == Scope::User || (i.flatpak.is_none() && i.profile.is_none()),
                "item {} comes from Flathub or a flake, which are per user, so it can't be installed for everyone",
                i.id
            );
            for a in &i.aliases {
                ensure!(
                    !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                    "item {}: not a name: {a}",
                    i.id
                );
            }
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

    /// What `name` stands for: an item's id first, then any item answering to it.
    pub fn resolve(&self, name: &str) -> Found<'_> {
        if let Some(i) = self.items.iter().find(|i| i.id.eq_ignore_ascii_case(name)) {
            return Found::One(i);
        }
        let found: Vec<&Item> = self.items.iter().filter(|i| i.answers_to(name)).collect();
        match found.len() {
            0 => Found::None,
            1 => Found::One(found[0]),
            _ => Found::Many(found),
        }
    }

    /// Items matching every word of `term`, in their names or summaries; exact names
    /// first.
    pub fn search(&self, term: &str) -> Vec<&Item> {
        let words: Vec<String> = term.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(u8, &Item)> = self
            .items
            .iter()
            .filter_map(|i| {
                let text = format!("{} {} {}", i.names().join(" "), i.summary.to_lowercase(), i.category);
                if !words.iter().all(|w| text.contains(w.as_str())) {
                    return None;
                }
                let rank = if i.answers_to(term.trim()) {
                    0
                } else if words.iter().all(|w| i.names().iter().any(|n| n.contains(w.as_str()))) {
                    1
                } else {
                    2
                };
                Some((rank, i))
            })
            .collect();
        hits.sort_by_key(|(rank, _)| *rank);
        hits.into_iter().map(|(_, i)| i).collect()
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
            if i.unfree {
                consent.insert(Consent::License { item: i.name.clone() });
            }
            if i.scope() == Scope::User {
                for attr in i.profile_attrs() {
                    let r = have.reference(&attr);
                    if !have.profile_attrs.contains(&attr) && !plan.profiles.contains(&r) {
                        plan.profiles.push(r);
                    }
                }
            } else {
                for p in &i.packages {
                    if !packages.contains(p) && !have.settings.packages.contains(p) {
                        packages.push(p.clone());
                        if i.unfree {
                            unfree.push(p.clone());
                        }
                    }
                }
                nyx.extend(i.nyx.iter().filter(|n| !have.nyx_apps.contains(*n)).cloned());
            }
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

    /// What removing `ids` takes, wherever they're installed: their packages and Nyx apps
    /// (the system's and the user's profile), and Flathub apps. Services and groups stay,
    /// since something else may use them.
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
            for attr in i.profile_attrs() {
                for e in have.profile.iter().filter(|e| e.package() == Some(attr.as_str())) {
                    let r = e.reference();
                    if !plan.profiles.contains(&r) {
                        plan.profiles.push(r);
                    }
                }
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

    /// Per-user items installed for everyone (before NyxOS installed them per user).
    pub fn system_wide<'a>(&'a self, have: &'a Installed) -> impl Iterator<Item = &'a Item> + 'a {
        self.items.iter().filter(|i| i.scope() == Scope::User && have.system_wide(i))
    }

    /// What moving `ids` from everyone's packages into the user's own profile takes: the
    /// profile packages, to add first, then the System Manager changes that remove the
    /// system's copies (one rebuild). Other accounts lose the system's copy.
    pub fn moving(&self, ids: &BTreeSet<String>, have: &Installed) -> Result<Plan> {
        let mut plan = Plan::default();
        let mut packages = Vec::new();
        let mut nyx = Vec::new();
        for id in ids {
            let Some(i) = self.item(id) else { bail!("no item {id} in the catalog") };
            ensure!(i.scope() == Scope::User, "{} is part of the system, so it stays installed for everyone", i.name);
            ensure!(have.system_wide(i), "{} isn't installed for everyone", i.name);
            for attr in i.profile_attrs() {
                let r = have.reference(&attr);
                if !have.profile_attrs.contains(&attr) && !plan.profiles.contains(&r) {
                    plan.profiles.push(r);
                }
            }
            if i.unfree {
                plan.consent.push(Consent::License { item: i.name.clone() });
            }
            packages.extend(i.packages.iter().filter(|p| have.settings.packages.contains(*p)).cloned());
            nyx.extend(i.nyx.iter().filter(|n| have.nyx_apps.contains(*n)).cloned());
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
    /// Flakes in the user's Nix profile, as they were added ([`ProfileElement::reference`]).
    pub profiles: BTreeSet<String>,
    /// The user's Nix profile.
    pub profile: Vec<ProfileElement>,
    /// The packages in it, by attribute (`firefox`, `nyx-notepad`).
    pub profile_attrs: BTreeSet<String>,
    /// Commands on the system's and the user's paths.
    pub bins: BTreeSet<String>,
    /// The channel per-user packages come from ([`channel`]); [`DEFAULT_CHANNEL`] when
    /// unknown.
    pub channel: Option<String>,
}

impl Installed {
    /// `CHANNEL#attr`: how a per-user package is added to a profile.
    pub fn reference(&self, attr: &str) -> String {
        format!("{}#{attr}", self.channel.as_deref().unwrap_or(DEFAULT_CHANNEL))
    }

    /// Whether a per-user `item` is installed for everyone instead (NyxOS before
    /// per-user installs, or someone who installed it that way).
    pub fn system_wide(&self, item: &Item) -> bool {
        let packages = !item.packages.is_empty() && item.packages.iter().all(|p| self.settings.packages.contains(p));
        let nyx = !item.nyx.is_empty() && item.nyx.iter().all(|n| self.nyx_apps.contains(n));
        packages || nyx
    }

    /// Set the user's profile (and what's in it).
    pub fn with_profile(mut self, profile: Vec<ProfileElement>) -> Installed {
        self.profiles = profile.iter().map(ProfileElement::reference).collect();
        self.profile_attrs = profile.iter().filter_map(|e| e.package().map(String::from)).collect();
        self.profile = profile;
        self
    }
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
        Installed { settings, nyx_apps, flatpaks, bins, channel: channel(), ..Installed::default() }.with_profile(profile())
    }

    /// Whether `item` is installed, for everyone or in the user's profile.
    pub fn has(&self, item: &Item) -> bool {
        if !item.nyx.is_empty() {
            return item.nyx.iter().all(|n| self.nyx_apps.contains(n) || self.profile_attrs.contains(&format!("nyx-{n}")));
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
            return item.packages.iter().all(|p| self.settings.packages.contains(p) || self.profile_attrs.contains(p));
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

/// An element of the user's Nix profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileElement {
    /// Its name in the profile (`nix profile upgrade NAME`).
    pub name: String,
    /// The flake it was added from, as given (`github:nyxos-project/nyxos/stable`).
    pub url: String,
    /// `legacyPackages.x86_64-linux.firefox`.
    pub attr_path: Option<String>,
    pub store_paths: Vec<String>,
}

impl ProfileElement {
    /// The package's attribute: `firefox` for `legacyPackages.x86_64-linux.firefox`.
    pub fn package(&self) -> Option<&str> {
        let a = self.attr_path.as_deref()?;
        let mut parts = a.splitn(3, '.');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("legacyPackages" | "packages"), Some(_), Some(rest)) => Some(rest),
            _ => Some(a),
        }
    }

    /// `URL#attr`, or the flake alone for its default package: what it was added as.
    pub fn reference(&self) -> String {
        match self.package() {
            Some("default") | None => self.url.clone(),
            Some(p) => format!("{}#{p}", self.url),
        }
    }

    /// The version in its store path's name (`2.1.152` for `…-claude-code-2.1.152`).
    pub fn version(&self) -> Option<String> {
        let path = self.store_paths.first()?;
        let name = path.rsplit('/').next()?.split_once('-')?.1;
        drv_version(name)
    }
}

/// The version part of a derivation name, as Nix splits it: from the first `-` followed by
/// something other than a letter.
pub fn drv_version(name: &str) -> Option<String> {
    let b = name.as_bytes();
    (0..b.len().saturating_sub(1))
        .find(|&i| b[i] == b'-' && !b[i + 1].is_ascii_alphabetic())
        .map(|i| name[i + 1..].to_string())
        .filter(|v| !v.is_empty())
}

/// The user's Nix profile.
pub fn profile() -> Vec<ProfileElement> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        elements: BTreeMap<String, Element>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Element {
        original_url: Option<String>,
        attr_path: Option<String>,
        #[serde(default)]
        store_paths: Vec<String>,
    }
    let Ok(out) = std::process::Command::new("nix").args(["profile", "list", "--json"]).output() else {
        return Vec::new();
    };
    let Ok(list) = serde_json::from_slice::<List>(&out.stdout) else { return Vec::new() };
    list.elements
        .into_iter()
        .filter_map(|(name, e)| {
            Some(ProfileElement { name, url: e.original_url?, attr_path: e.attr_path, store_paths: e.store_paths })
        })
        .collect()
}

/// The user's Nix profile: each element's name and what it was added as
/// ([`ProfileElement::reference`]).
pub fn profile_elements() -> BTreeMap<String, String> {
    profile().into_iter().map(|e| (e.name.clone(), e.reference())).collect()
}

/// The channel the system follows (`github:nyxos-project/nyxos/stable`), from the
/// `nyxos` input of the system's flake.
pub fn channel() -> Option<String> {
    let text = std::fs::read_to_string(SYSTEM_FLAKE_LOCK).ok()?;
    channel_from_lock(&text)
}

/// [`channel`], from a `flake.lock`'s text.
pub fn channel_from_lock(text: &str) -> Option<String> {
    let lock: serde_json::Value = serde_json::from_str(text).ok()?;
    let root = lock["root"].as_str().unwrap_or("root");
    let node = lock["nodes"][root]["inputs"]["nyxos"].as_str()?;
    let original = &lock["nodes"][node]["original"];
    if original["type"].as_str()? != "github" {
        return None;
    }
    let (owner, repo) = (original["owner"].as_str()?, original["repo"].as_str()?);
    Some(match original["ref"].as_str() {
        Some(r) => format!("github:{owner}/{repo}/{r}"),
        None => format!("github:{owner}/{repo}"),
    })
}

/// What a package starts with the session (`share/nyx/autostart`: `UNIT TARGET` lines).
pub fn session_units(package: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(package.join("share/nyx/autostart"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(u, t)| (u.trim().to_string(), t.trim().to_string())))
        .filter(|(u, t)| !u.is_empty() && !t.is_empty())
        .collect()
}

/// How the user's session follows a change to their profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileChange {
    /// Packages were added: start what they run with the session.
    Added,
    /// Packages were upgraded: restart what's running, so it's the new version.
    Upgraded,
    /// Packages were removed (their services were stopped first).
    Removed,
}

/// After the profile changed: ask the System Manager to trust its Nyx apps (Accounts,
/// Clipboard and app actions know programs by the folders NyxOS lists), reload the
/// user's systemd (NyxOS's generator links profile apps' units), then start or restart
/// the apps' session services. Each step is best effort.
pub fn after_profile_change(change: ProfileChange) {
    use std::process::{Command, Stdio};
    let quiet = |program: &str, args: &[&str]| {
        let _ = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    };
    quiet(
        "busctl",
        &[
            "--system",
            "call",
            nyx_system_proto::BUS_NAME,
            nyx_system_proto::OBJECT_PATH,
            "io.github.nyxos_project.System1",
            "TrustApps",
        ],
    );
    quiet("systemctl", &["--user", "daemon-reload"]);
    if change == ProfileChange::Removed {
        return;
    }
    // Nyx Icons recolours the new launchers for the theme.
    quiet("systemctl", &["--user", "start", "--no-block", "nyx-icons.service"]);
    let mut units: Vec<(String, String)> = Vec::new();
    for e in profile() {
        for p in &e.store_paths {
            units.extend(session_units(Path::new(p)));
        }
    }
    for (unit, target) in units {
        let unit = if unit.contains('.') { unit } else { format!("{unit}.service") };
        let active =
            Command::new("systemctl").args(["--user", "is-active", "--quiet", &target]).status().is_ok_and(|s| s.success());
        match change {
            ProfileChange::Added if active => quiet("systemctl", &["--user", "start", "--no-block", &unit]),
            ProfileChange::Upgraded => quiet("systemctl", &["--user", "try-restart", "--no-block", &unit]),
            _ => {}
        }
    }
}

/// Add flakes to the user's Nix profile, or remove them. `line` gets nix's output.
pub fn nix_profile(refs: &[String], install: bool, mut line: impl FnMut(&str)) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    if install {
        for r in refs {
            ensure!(valid_flake_ref(r), "not a flake reference Nyx installs from: {r}");
        }
    }
    let mut runs: Vec<Vec<String>> = Vec::new();
    let mut stopping: Vec<String> = Vec::new();
    if install {
        runs.extend(refs.iter().map(|r| vec!["add".to_string(), r.clone()]));
    } else {
        let leaving: Vec<ProfileElement> = profile().into_iter().filter(|e| refs.contains(&e.reference())).collect();
        for e in &leaving {
            for p in &e.store_paths {
                stopping.extend(session_units(Path::new(p)).into_iter().map(|(u, _)| u));
            }
        }
        if !leaving.is_empty() {
            runs.push([vec!["remove".to_string()], leaving.into_iter().map(|e| e.name).collect()].concat());
        }
    }
    for unit in &stopping {
        let _ = Command::new("systemctl").args(["--user", "stop", "--no-block", unit]).status();
    }
    let changed = !runs.is_empty();
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
    if changed {
        after_profile_change(if install { ProfileChange::Added } else { ProfileChange::Removed });
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
    fn apps_are_the_users_and_only_the_system_part_rebuilds() {
        let c = catalog();
        let pick: BTreeSet<String> =
            ["docker", "vscode", "nyx-mail", "steam", "firefox", "nyx-assistant"].map(String::from).into();
        let have = Installed { channel: Some("github:nyxos-project/nyxos/testing".into()), ..Installed::default() };
        let plan = c.plan(&pick, "ken", &have).unwrap();
        // Docker turns on a service and a group, and the Assistant is part of the desktop.
        assert_eq!(plan.changes[0], Change::AddPackages { packages: vec!["docker-compose".into()], unfree: vec![] });
        assert!(plan.changes.contains(&Change::NyxApp { name: "assistant".into(), enabled: Some(true) }));
        assert!(plan.changes.contains(&Change::Service { name: "docker".into(), enabled: Some(true) }));
        assert!(plan.changes.contains(&Change::GroupMember { group: "docker".into(), user: "ken".into(), member: true }));
        assert!(!plan.changes.contains(&Change::NyxApp { name: "mail".into(), enabled: Some(true) }));
        // Firefox, VS Code and Nyx Mail are the user's, from the system's channel.
        for attr in ["firefox", "vscode", "nyx-mail"] {
            assert!(plan.profiles.contains(&format!("github:nyxos-project/nyxos/testing#{attr}")), "{attr}: {:?}", plan.profiles);
        }
        assert_eq!(plan.flatpaks, vec!["com.valvesoftware.Steam".to_string()]);
        assert!(plan.consent.iter().any(|c| matches!(c, Consent::Group { group, .. } if group == "docker")));
        assert_eq!(plan.consent.iter().filter(|c| matches!(c, Consent::License { .. })).count(), 2);
        for ch in &plan.changes {
            ch.validate().unwrap();
        }
        for r in &plan.profiles {
            assert!(valid_flake_ref(r), "{r}");
        }
    }

    #[test]
    fn scopes() {
        let c = catalog();
        let scope = |id: &str| c.item(id).unwrap().scope();
        for id in ["firefox", "claude-code", "nyx-notepad", "nyx-mail", "nyx-weather", "oh-my-pi", "steam"] {
            assert_eq!(scope(id), Scope::User, "{id}");
        }
        for id in ["nyx-settings", "nyx-files", "nyx-capture", "nyx-backup", "nyx-assistant", "nyx-clipboard", "docker", "ollama"]
        {
            assert_eq!(scope(id), Scope::System, "{id}");
        }
        let base = "[[section]]\nid='s'\nname='S'\nsummary=''\n[[category]]\nid='c'\nname='C'\nsection='s'\n";
        let item = "[[item]]\nid='x'\nname='X'\nsummary=''\ncategory='c'\n";
        assert!(Catalog::parse(&format!("{base}{item}tier='welcome'\nscope='user'\nservices=['docker']\n")).is_err());
        assert!(Catalog::parse(&format!("{base}{item}tier='base'\nscope='user'\npackages=['a']\n")).is_err());
        assert!(Catalog::parse(&format!("{base}{item}tier='welcome'\nscope='system'\nflatpak='a.b.C'\n")).is_err());
        assert!(Catalog::parse(&format!("{base}{item}tier='welcome'\npackages=['a']\naliases=['b c']\n")).is_err());
    }

    #[test]
    fn names_resolve_the_same_everywhere() {
        let c = catalog();
        for name in ["claude", "claude-code", "Claude Code", "CLAUDE"] {
            assert!(matches!(c.resolve(name), Found::One(i) if i.id == "claude-code"), "{name}");
        }
        assert!(matches!(c.resolve("notepad"), Found::One(i) if i.id == "nyx-notepad"));
        assert!(matches!(c.resolve("cursor-agent"), Found::One(i) if i.id == "cursor-agent"));
        assert!(matches!(c.resolve("cursor"), Found::One(i) if i.id == "cursor"), "an id wins");
        assert_eq!(c.resolve("no-such-thing"), Found::None);
        let hits = c.search("claude");
        assert_eq!(hits.first().map(|i| i.id.as_str()), Some("claude-code"));
        assert!(c.search("coding agent").iter().any(|i| i.id == "codex"));
        assert!(c.search("").is_empty());
    }

    #[test]
    fn profile_elements_and_versions() {
        let e = ProfileElement {
            name: "claude-code".into(),
            url: "github:nyxos-project/nyxos/stable".into(),
            attr_path: Some("legacyPackages.x86_64-linux.claude-code".into()),
            store_paths: vec!["/nix/store/abc-claude-code-2.1.152".into()],
        };
        assert_eq!(e.package(), Some("claude-code"));
        assert_eq!(e.reference(), "github:nyxos-project/nyxos/stable#claude-code");
        assert_eq!(e.version().as_deref(), Some("2.1.152"));
        let omp = ProfileElement {
            url: "github:can1357/oh-my-pi".into(),
            attr_path: Some("packages.x86_64-linux.default".into()),
            ..ProfileElement::default()
        };
        assert_eq!(omp.reference(), "github:can1357/oh-my-pi");
        assert_eq!(drv_version("nyx-notepad-0.1.0-unstable-2026-11-01").as_deref(), Some("0.1.0-unstable-2026-11-01"));
        assert_eq!(drv_version("hello"), None);
        let kde = ProfileElement { attr_path: Some("legacyPackages.x86_64-linux.kdePackages.kdenlive".into()), ..e };
        assert_eq!(kde.package(), Some("kdePackages.kdenlive"));
    }

    #[test]
    fn the_channel_comes_from_the_system_flake() {
        let lock = r#"{"nodes":{"root":{"inputs":{"nyxos":"nyxos_2"}},"nyxos_2":{"original":{"type":"github","owner":"nyxos-project","repo":"nyxos","ref":"testing"}}},"root":"root","version":7}"#;
        assert_eq!(channel_from_lock(lock).as_deref(), Some("github:nyxos-project/nyxos/testing"));
        assert_eq!(channel_from_lock(r#"{"nodes":{"root":{}},"root":"root"}"#), None);
    }

    #[test]
    fn moving_to_the_users_profile() {
        let c = catalog();
        let mut have = Installed::default();
        have.nyx_apps.insert("notepad".into());
        have.settings.packages.insert("firefox".into());
        let ids: BTreeSet<String> = ["nyx-notepad", "firefox"].map(String::from).into();
        assert_eq!(c.system_wide(&have).map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["nyx-notepad", "firefox"]);
        let plan = c.moving(&ids, &have).unwrap();
        assert_eq!(plan.profiles, vec![format!("{DEFAULT_CHANNEL}#firefox"), format!("{DEFAULT_CHANNEL}#nyx-notepad")]);
        assert_eq!(
            plan.changes,
            vec![
                Change::RemovePackages { packages: vec!["firefox".into()] },
                Change::NyxApp { name: "notepad".into(), enabled: Some(false) },
            ]
        );
        assert!(c.moving(&["nyx-settings".to_string()].into(), &have).is_err(), "part of the system");
        assert!(c.moving(&["gimp".to_string()].into(), &have).is_err(), "not installed for everyone");
    }

    #[test]
    fn profile_packages_count_as_installed_and_are_removed_from_the_profile() {
        let c = catalog();
        let have = Installed::default().with_profile(vec![ProfileElement {
            name: "firefox".into(),
            url: "flake:nixpkgs".into(),
            attr_path: Some("legacyPackages.x86_64-linux.firefox".into()),
            store_paths: vec![],
        }]);
        assert!(have.has(c.item("firefox").unwrap()));
        let ids: BTreeSet<String> = ["firefox".to_string()].into();
        assert!(c.plan(&ids, "ken", &have).unwrap().is_empty());
        assert_eq!(c.removal(&ids, &have).unwrap().profiles, vec!["flake:nixpkgs#firefox".to_string()]);
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

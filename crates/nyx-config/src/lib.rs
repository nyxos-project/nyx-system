//! NyxOS's three configuration layers, merged:
//!
//! 1. Distribution defaults, `/etc/nyxos/defaults/DOMAIN.toml` (written by the NixOS
//!    configuration, read-only).
//! 2. System overrides, `/etc/nyxos/overrides/DOMAIN.toml` (the System Manager writes
//!    these for the administrator).
//! 3. User settings, `~/.config/nyx/DOMAIN.toml` (the user's apps write these).
//!
//! Later layers win, key by key: tables merge, and any other value replaces the one
//! below it. Updates replace only the first layer. A domain is one file name, such as
//! `appearance` or `desktop`.

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use toml::{Table, Value};

pub mod store;
#[cfg(feature = "watch")]
pub mod watch;

pub use store::{Domain, Explained, Loaded, Migration, Problem, SCHEMA_KEY, Store, drop_defaults, parse, sparsify, table_of};

/// Which layer a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Defaults,
    System,
    User,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Defaults => "defaults",
            Layer::System => "system",
            Layer::User => "user",
        }
    }
}

/// Where the layers live. [`Layers::standard`] is the system's; tests and the System
/// Manager's checks use their own.
#[derive(Debug, Clone)]
pub struct Layers {
    pub defaults: PathBuf,
    pub system: PathBuf,
    pub user: PathBuf,
}

impl Layers {
    /// The NyxOS layers for the current user. `NYX_CONFIG_DEFAULTS`, `NYX_CONFIG_SYSTEM` and
    /// `XDG_CONFIG_HOME` move them.
    pub fn standard() -> Self {
        let env = |key: &str, fallback: &str| std::env::var_os(key).map(PathBuf::from).unwrap_or_else(|| fallback.into());
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
        Layers {
            defaults: env("NYX_CONFIG_DEFAULTS", "/etc/nyxos/defaults"),
            system: env("NYX_CONFIG_SYSTEM", "/etc/nyxos/overrides"),
            user: config_home.join("nyx"),
        }
    }

    fn dir(&self, layer: Layer) -> &Path {
        match layer {
            Layer::Defaults => &self.defaults,
            Layer::System => &self.system,
            Layer::User => &self.user,
        }
    }

    pub fn file(&self, layer: Layer, domain: &str) -> Result<PathBuf> {
        ensure!(valid_domain(domain), "invalid settings domain: {domain}");
        Ok(self.dir(layer).join(format!("{domain}.toml")))
    }

    /// One layer of a domain; a missing file is empty.
    pub fn read(&self, layer: Layer, domain: &str) -> Result<Table> {
        let path = self.file(layer, domain)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => text.parse::<Table>().with_context(|| format!("{} is not valid TOML", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Table::new()),
            Err(e) => Err(e).with_context(|| format!("can't read {}", path.display())),
        }
    }

    /// The domain with every layer applied.
    pub fn merged(&self, domain: &str) -> Result<Table> {
        let mut out = Table::new();
        for layer in [Layer::Defaults, Layer::System, Layer::User] {
            merge(&mut out, self.read(layer, domain)?);
        }
        Ok(out)
    }

    /// A value by dotted key (`colors.accent`), with the layer it comes from.
    pub fn get(&self, domain: &str, key: &str) -> Result<Option<(Value, Layer)>> {
        let path = key_path(key)?;
        for layer in [Layer::User, Layer::System, Layer::Defaults] {
            if let Some(v) = lookup(&self.read(layer, domain)?, &path) {
                return Ok(Some((v.clone(), layer)));
            }
        }
        Ok(None)
    }

    /// Set a key in one writable layer (system or user), keeping the rest of the file.
    pub fn set(&self, layer: Layer, domain: &str, key: &str, value: Value) -> Result<()> {
        ensure!(layer != Layer::Defaults, "distribution defaults are read-only");
        let path = key_path(key)?;
        let mut table = self.read(layer, domain)?;
        let (last, parents) = path.split_last().expect("key_path is never empty");
        let mut at = &mut table;
        for part in parents {
            let entry = at.entry(part.to_string()).or_insert_with(|| Value::Table(Table::new()));
            let Value::Table(next) = entry else { bail!("{key}: {part} isn't a table") };
            at = next;
        }
        at.insert(last.to_string(), value);
        write(&self.file(layer, domain)?, &table)
    }

    /// Remove a key from one writable layer, so the one below shows through again.
    pub fn reset(&self, layer: Layer, domain: &str, key: &str) -> Result<()> {
        ensure!(layer != Layer::Defaults, "distribution defaults are read-only");
        let path = key_path(key)?;
        let mut table = self.read(layer, domain)?;
        let (last, parents) = path.split_last().expect("key_path is never empty");
        let mut at = &mut table;
        for part in parents {
            match at.get_mut(*part) {
                Some(Value::Table(next)) => at = next,
                _ => return Ok(()),
            }
        }
        if at.remove(*last).is_some() {
            write(&self.file(layer, domain)?, &table)?;
        }
        Ok(())
    }
}

/// Domains are plain names: letters, digits and `-`.
pub fn valid_domain(domain: &str) -> bool {
    !domain.is_empty() && domain.len() <= 64 && domain.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn key_path(key: &str) -> Result<Vec<&str>> {
    let parts: Vec<&str> = key.split('.').collect();
    ensure!(
        !key.is_empty()
            && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))),
        "invalid settings key: {key}"
    );
    Ok(parts)
}

fn lookup<'a>(table: &'a Table, path: &[&str]) -> Option<&'a Value> {
    let (last, parents) = path.split_last()?;
    let mut at = table;
    for part in parents {
        at = at.get(*part)?.as_table()?;
    }
    at.get(*last)
}

/// Tables merge key by key; anything else replaces what's below.
pub fn merge(into: &mut Table, over: Table) {
    for (key, value) in over {
        match (into.get_mut(&key), value) {
            (Some(Value::Table(below)), Value::Table(above)) => merge(below, above),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

/// Replace the file atomically.
fn write(path: &Path, table: &Table) -> Result<()> {
    let dir = path.parent().context("settings file has no folder")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("settings"), std::process::id()));
    std::fs::write(&tmp, toml::to_string(table)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(dir: &Path) -> Layers {
        Layers { defaults: dir.join("defaults"), system: dir.join("system"), user: dir.join("user") }
    }

    #[test]
    fn later_layers_win_key_by_key() {
        let d = tempfile::tempdir().unwrap();
        let l = layers(d.path());
        std::fs::create_dir_all(&l.defaults).unwrap();
        std::fs::write(
            l.defaults.join("appearance.toml"),
            "theme = \"nyx-dark\"\n[fonts]\nui = \"Inter\"\nmono = \"JetBrains Mono\"\n",
        )
        .unwrap();
        l.set(Layer::System, "appearance", "fonts.ui", Value::String("Noto Sans".into())).unwrap();
        l.set(Layer::User, "appearance", "theme", Value::String("nord".into())).unwrap();
        let m = l.merged("appearance").unwrap();
        assert_eq!(m["theme"].as_str(), Some("nord"));
        assert_eq!(m["fonts"]["ui"].as_str(), Some("Noto Sans"));
        assert_eq!(m["fonts"]["mono"].as_str(), Some("JetBrains Mono"));
        assert_eq!(l.get("appearance", "fonts.ui").unwrap().unwrap().1, Layer::System);
        assert_eq!(l.get("appearance", "theme").unwrap().unwrap().1, Layer::User);
        l.reset(Layer::User, "appearance", "theme").unwrap();
        assert_eq!(l.get("appearance", "theme").unwrap().unwrap(), (Value::String("nyx-dark".into()), Layer::Defaults));
    }

    #[test]
    fn defaults_are_read_only_and_names_are_checked() {
        let d = tempfile::tempdir().unwrap();
        let l = layers(d.path());
        assert!(l.set(Layer::Defaults, "appearance", "theme", Value::Boolean(true)).is_err());
        assert!(l.read(Layer::User, "../etc/passwd").is_err());
        assert!(l.get("appearance", "a..b").is_err());
        assert!(l.merged("missing").unwrap().is_empty());
    }
}

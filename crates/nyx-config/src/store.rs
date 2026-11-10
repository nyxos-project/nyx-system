//! One domain's settings, versioned: what a component reads its preferences through.
//!
//! A [`Store`] adds to [`Layers`]:
//!
//! - a schema version (`schema = N` in the user's file) and migrations that bring an older
//!   file up to date, after keeping a copy of it (`DOMAIN.toml.vN.bak`);
//! - a user file that isn't valid TOML set aside as `DOMAIN.toml.broken-<time>` (never
//!   deleted), so the rest of the settings still load;
//! - the component's own built-in values as the lowest layer, so the user's file holds only
//!   what the user changed ([`Store::save`]) and a new NyxOS default reaches everyone who
//!   didn't;
//! - [`Store::explain`], where a value comes from and what it would be after a reset.

use crate::{Layer, Layers, merge};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// The key holding a file's schema version. It never reaches a component.
pub const SCHEMA_KEY: &str = "schema";

/// Brings a user file from one schema version to the next. `builtin` is the component's
/// own values: what every setting was by default when a file from before was written
/// (NyxOS's defaults didn't reach it then), for migrations that drop values equal to it.
pub type Migration = fn(user: &mut Table, builtin: &Table);

/// A settings domain and its schema history.
#[derive(Debug, Clone, Copy)]
pub struct Domain {
    /// The file name, as in `/etc/nyxos/defaults/NAME.toml`.
    pub name: &'static str,
    /// The current schema version. A file without `schema` is version 0.
    pub version: u32,
    /// `migrations[n]` takes a file from version `n` to `n + 1`; there are `version` of them.
    pub migrations: &'static [Migration],
}

/// A problem found while loading, reported rather than failing the load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub layer: Layer,
    pub message: String,
}

/// Where one setting's value comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Explained {
    /// The value in effect, and its layer (`None` for the component's built-in value).
    pub effective: Option<Value>,
    pub layer: Option<Layer>,
    /// What a reset would leave: the system override, else the NyxOS default, else the
    /// built-in value.
    pub default: Option<Value>,
    /// The user's own value, if they set one.
    pub user: Option<Value>,
}

impl Explained {
    pub fn is_overridden(&self) -> bool {
        self.user.is_some() && self.user != self.default
    }
}

/// The merged settings of one domain.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub table: Table,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub layers: Layers,
    pub domain: Domain,
    builtin: Table,
}

impl Store {
    /// The domain over the system's layers. `builtin` is the component's own value for
    /// every key (its `Default`, serialized), the layer below NyxOS's defaults.
    pub fn new(domain: Domain, builtin: Table) -> Self {
        Store::with_layers(Layers::standard(), domain, builtin)
    }

    pub fn with_layers(layers: Layers, domain: Domain, builtin: Table) -> Self {
        Store { layers, domain, builtin }
    }

    pub fn user_file(&self) -> PathBuf {
        self.layers.file(Layer::User, self.domain.name).expect("domain names are checked by the component")
    }

    /// Everything below the user's layer.
    pub fn base(&self) -> Loaded {
        let mut out = Loaded { table: self.builtin.clone(), problems: Vec::new() };
        for layer in [Layer::Defaults, Layer::System] {
            match self.layers.read(layer, self.domain.name) {
                Ok(mut t) => {
                    t.remove(SCHEMA_KEY);
                    merge(&mut out.table, t);
                }
                Err(e) => out.problems.push(Problem { layer, message: format!("{e:#}") }),
            }
        }
        out
    }

    /// The user's own values, migrated to the current schema (and the file rewritten, after
    /// a backup) or, if the file isn't valid TOML, set aside and treated as empty.
    pub fn user(&self, problems: &mut Vec<Problem>) -> Table {
        let path = self.user_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Table::new(),
            Err(e) => {
                problems.push(Problem { layer: Layer::User, message: format!("can't read {}: {e}", path.display()) });
                return Table::new();
            }
        };
        let mut table = match text.parse::<Table>() {
            Ok(t) => t,
            Err(e) => {
                let kept = set_aside(&path);
                problems.push(Problem {
                    layer: Layer::User,
                    message: match kept {
                        Ok(to) => format!("{} isn't valid TOML ({e}); kept as {}", path.display(), to.display()),
                        Err(k) => format!("{} isn't valid TOML ({e}); couldn't set it aside: {k:#}", path.display()),
                    },
                });
                return Table::new();
            }
        };
        let from = table.get(SCHEMA_KEY).and_then(Value::as_integer).unwrap_or(0).max(0) as u32;
        if from < self.domain.version {
            if let Err(e) = self.migrate(&path, &mut table, from) {
                problems.push(Problem { layer: Layer::User, message: format!("{e:#}") });
            }
        } else if from > self.domain.version {
            problems.push(Problem {
                layer: Layer::User,
                message: format!(
                    "{} is schema {from}, newer than this program's {}; read as is and left alone",
                    path.display(),
                    self.domain.version
                ),
            });
        }
        table.remove(SCHEMA_KEY);
        table
    }

    fn migrate(&self, path: &Path, table: &mut Table, from: u32) -> Result<()> {
        let backup = path.with_file_name(format!("{}.toml.v{from}.bak", self.domain.name));
        std::fs::copy(path, &backup).with_context(|| format!("can't back up {} before migrating it", path.display()))?;
        for step in from..self.domain.version {
            if let Some(m) = self.domain.migrations.get(step as usize) {
                m(table, &self.builtin);
            }
        }
        table.insert(SCHEMA_KEY.into(), Value::Integer(self.domain.version.into()));
        crate::write(path, table)
    }

    /// Built-in values, then every layer.
    pub fn load(&self) -> Loaded {
        let mut out = self.base();
        let user = self.user(&mut out.problems);
        merge(&mut out.table, user);
        out
    }

    pub fn explain(&self, key: &str) -> Result<Explained> {
        let path = crate::key_path(key)?;
        let mut problems = Vec::new();
        let base = self.base();
        let user = self.user(&mut problems);
        let user_value = crate::lookup(&user, &path).cloned();
        let mut layer = None;
        for l in [Layer::System, Layer::Defaults] {
            if self.layers.read(l, self.domain.name).ok().and_then(|t| crate::lookup(&t, &path).cloned()).is_some() {
                layer = Some(l);
                break;
            }
        }
        let default = crate::lookup(&base.table, &path).cloned();
        Ok(Explained {
            effective: user_value.clone().or_else(|| default.clone()),
            layer: if user_value.is_some() { Some(Layer::User) } else { layer },
            default,
            user: user_value,
        })
    }

    /// Set one key in the user's file.
    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        self.stamp()?;
        self.layers.set(Layer::User, self.domain.name, key, value)
    }

    /// Hand one key back to the layers below.
    pub fn reset(&self, key: &str) -> Result<()> {
        self.layers.reset(Layer::User, self.domain.name, key)
    }

    /// Hand every key back: the user's file is kept as `DOMAIN.toml.reset.bak` and removed.
    pub fn reset_all(&self) -> Result<()> {
        let path = self.user_file();
        if path.exists() {
            std::fs::rename(&path, path.with_file_name(format!("{}.toml.reset.bak", self.domain.name)))?;
        }
        Ok(())
    }

    /// Save a component's whole settings: the user's file becomes only what differs from
    /// the layers below, so values left at their default follow the default.
    pub fn save(&self, settings: &Table) -> Result<()> {
        let base = self.base();
        let mut user = settings.clone();
        user.remove(SCHEMA_KEY);
        sparsify(&mut user, &base.table);
        user.insert(SCHEMA_KEY.into(), Value::Integer(self.domain.version.into()));
        crate::write(&self.user_file(), &user)
    }

    /// Make sure the user's file is at the current schema before writing into it.
    fn stamp(&self) -> Result<()> {
        let mut problems = Vec::new();
        let mut user = self.user(&mut problems);
        let path = self.user_file();
        let current = std::fs::read_to_string(&path).ok().and_then(|t| t.parse::<Table>().ok());
        let stamped = current
            .as_ref()
            .and_then(|t| t.get(SCHEMA_KEY))
            .and_then(Value::as_integer)
            .is_some_and(|v| v >= self.domain.version as i64);
        if !stamped {
            user.insert(SCHEMA_KEY.into(), Value::Integer(self.domain.version.into()));
            crate::write(&path, &user)?;
        }
        Ok(())
    }
}

/// Remove from `user` every value equal to the one in `base`, and tables left empty.
pub fn sparsify(user: &mut Table, base: &Table) {
    user.retain(|key, value| match (value, base.get(key)) {
        (Value::Table(t), Some(Value::Table(b))) => {
            sparsify(t, b);
            !t.is_empty()
        }
        (v, Some(b)) => !same(v, b),
        (_, None) => true,
    });
}

/// Equal as settings: integers and floats compare by value.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Integer(x), Value::Float(y)) | (Value::Float(y), Value::Integer(x)) => (*x as f64 - y).abs() < 1e-9,
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b)),
        (Value::Table(x), Value::Table(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// A migration from a file each component wrote whole to only what the user changed:
/// values equal to the component's own default go, and follow NyxOS's from now on.
pub fn drop_defaults(user: &mut Table, builtin: &Table) {
    sparsify(user, builtin);
}

fn set_aside(path: &Path) -> Result<PathBuf> {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.toml");
    let to = path.with_file_name(format!("{name}.broken-{secs}"));
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// Serialize a component's settings (its `Default`, or what it's about to [`Store::save`]).
pub fn table_of<T: serde::Serialize>(value: &T) -> Table {
    Table::try_from(value).unwrap_or_default()
}

/// Read merged settings into a component's type. A key of the wrong type falls back to
/// the component's built-in value for that key and is reported, instead of losing every
/// other setting.
pub fn parse<T: serde::de::DeserializeOwned + serde::Serialize + Default>(loaded: &Loaded) -> (T, Vec<String>) {
    if let Ok(v) = loaded.table.clone().try_into::<T>() {
        return (v, Vec::new());
    }
    let mut table = table_of(&T::default());
    let mut errors = Vec::new();
    for (key, value) in &loaded.table {
        let mut trial = table.clone();
        trial.insert(key.clone(), value.clone());
        match trial.clone().try_into::<T>() {
            Ok(_) => table = trial,
            Err(e) => errors.push(format!("{key}: {}", e.message())),
        }
    }
    (table.try_into::<T>().unwrap_or_default(), errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCK_V1: Domain = Domain { name: "dock", version: 1, migrations: &[drop_defaults] };

    fn store(dir: &Path, builtin: &str) -> Store {
        let layers = Layers { defaults: dir.join("defaults"), system: dir.join("system"), user: dir.join("user") };
        for d in [&layers.defaults, &layers.system, &layers.user] {
            std::fs::create_dir_all(d).unwrap();
        }
        Store::with_layers(layers, DOCK_V1, builtin.parse().unwrap())
    }

    #[test]
    fn precedence_and_reset_follow_a_new_default() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\nhide = \"smart\"\n");
        assert_eq!(s.load().table["icon_size"].as_integer(), Some(48));
        std::fs::write(s.layers.defaults.join("dock.toml"), "schema = 1\nicon_size = 42\n").unwrap();
        assert_eq!(s.load().table["icon_size"].as_integer(), Some(42));
        assert!(!s.load().table.contains_key(SCHEMA_KEY));
        s.set("icon_size", Value::Integer(64)).unwrap();
        let e = s.explain("icon_size").unwrap();
        assert!(e.is_overridden());
        assert_eq!((e.effective, e.layer, e.default), (Some(Value::Integer(64)), Some(Layer::User), Some(Value::Integer(42))));
        s.reset("icon_size").unwrap();
        std::fs::write(s.layers.defaults.join("dock.toml"), "icon_size = 40\n").unwrap();
        let e = s.explain("icon_size").unwrap();
        assert_eq!((e.effective, e.layer), (Some(Value::Integer(40)), Some(Layer::Defaults)));
        assert_eq!(s.explain("hide").unwrap().layer, None);
    }

    #[test]
    fn save_writes_only_what_differs() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\nhide = \"smart\"\npinned = [\"a\"]\n[monitors]\n");
        std::fs::write(s.layers.defaults.join("dock.toml"), "icon_size = 42\n").unwrap();
        let whole: Table = "icon_size = 42\nhide = \"never\"\npinned = [\"a\"]\n[monitors.x]\nenabled = true\n".parse().unwrap();
        s.save(&whole).unwrap();
        let user: Table = std::fs::read_to_string(s.user_file()).unwrap().parse().unwrap();
        assert_eq!(user.get("icon_size"), None);
        assert_eq!(user["hide"].as_str(), Some("never"));
        assert_eq!(user.get("pinned"), None);
        assert_eq!(user["monitors"]["x"]["enabled"].as_bool(), Some(true));
        assert_eq!(user[SCHEMA_KEY].as_integer(), Some(1));
    }

    #[test]
    fn an_old_file_is_backed_up_and_migrated() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\nhide = \"smart\"\n");
        std::fs::write(s.user_file(), "icon_size = 48\nhide = \"never\"\n").unwrap();
        let t = s.load().table;
        assert_eq!(t["hide"].as_str(), Some("never"));
        let user: Table = std::fs::read_to_string(s.user_file()).unwrap().parse().unwrap();
        assert_eq!(user.get("icon_size"), None, "a value equal to the built-in one follows the default");
        assert_eq!(user[SCHEMA_KEY].as_integer(), Some(1));
        let backup = std::fs::read_to_string(s.layers.user.join("dock.toml.v0.bak")).unwrap();
        assert!(backup.contains("icon_size = 48"));
        // Loading again doesn't migrate again.
        s.load();
        assert!(!s.layers.user.join("dock.toml.v1.bak").exists());
    }

    #[test]
    fn an_old_file_follows_nyxos_where_it_held_the_built_in_value() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\nhide = \"smart\"\n");
        std::fs::write(s.layers.defaults.join("dock.toml"), "schema = 1\nicon_size = 42\nhide = \"never\"\n").unwrap();
        // Written whole before NyxOS's defaults reached it: 48 and smart were just the dock's own.
        std::fs::write(s.user_file(), "icon_size = 48\nhide = \"always\"\n").unwrap();
        let t = s.load().table;
        assert_eq!(t["icon_size"].as_integer(), Some(42), "follows NyxOS's default");
        assert_eq!(t["hide"].as_str(), Some("always"), "the user's own choice stays");
    }

    #[test]
    fn a_broken_file_is_kept_and_the_rest_loads() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\n");
        std::fs::write(s.layers.defaults.join("dock.toml"), "icon_size = 42\n").unwrap();
        std::fs::write(s.user_file(), "icon_size = = 1\n").unwrap();
        let l = s.load();
        assert_eq!(l.table["icon_size"].as_integer(), Some(42));
        assert_eq!(l.problems.len(), 1);
        assert!(!s.user_file().exists());
        let kept: Vec<_> = std::fs::read_dir(&s.layers.user).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(kept.iter().any(|n| n.to_string_lossy().starts_with("dock.toml.broken-")));
    }

    #[test]
    fn a_broken_defaults_file_is_reported_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\n");
        std::fs::write(s.layers.defaults.join("dock.toml"), "[[[").unwrap();
        let l = s.load();
        assert_eq!(l.table["icon_size"].as_integer(), Some(48));
        assert_eq!(l.problems[0].layer, Layer::Defaults);
    }

    #[test]
    fn a_newer_file_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\n");
        std::fs::write(s.user_file(), "schema = 9\nicon_size = 48\nfuture = true\n").unwrap();
        let l = s.load();
        assert_eq!(l.table["future"].as_bool(), Some(true));
        assert!(std::fs::read_to_string(s.user_file()).unwrap().contains("schema = 9"));
        assert_eq!(l.problems.len(), 1);
    }

    #[test]
    fn invalid_keys_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "");
        assert!(s.set("a..b", Value::Boolean(true)).is_err());
        assert!(s.explain("").is_err());
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    #[serde(default)]
    struct Dock {
        icon_size: u32,
        hide: String,
    }

    impl Default for Dock {
        fn default() -> Self {
            Dock { icon_size: 48, hide: "smart".into() }
        }
    }

    #[test]
    fn parse_keeps_good_keys_when_one_is_wrong() {
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "");
        std::fs::write(s.user_file(), "schema = 1\nicon_size = \"huge\"\nhide = \"never\"\n").unwrap();
        let (dock, errors): (Dock, _) = parse(&s.load());
        assert_eq!(dock, Dock { icon_size: 48, hide: "never".into() });
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("icon_size"));
    }

    #[cfg(feature = "watch")]
    #[test]
    fn watch_sees_a_change_in_any_layer() {
        use std::sync::mpsc;
        use std::time::Duration;
        let d = tempfile::tempdir().unwrap();
        let s = store(d.path(), "icon_size = 48\n");
        let (tx, rx) = mpsc::channel();
        let _w = s.watch(move || {
            let _ = tx.send(());
        })
        .unwrap();
        std::fs::write(s.layers.defaults.join("bar.toml"), "x = 1\n").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "another domain's file isn't this one");
        s.set("icon_size", Value::Integer(40)).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
        while rx.try_recv().is_ok() {}
        std::fs::write(s.layers.defaults.join("dock.toml"), "icon_size = 42\n").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }
}

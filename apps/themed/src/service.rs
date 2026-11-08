//! `io.github.design_nexus.nyx.Theme1`: the theme engine's operations on the bus,
//! the current generation, and `ThemeChanged`. Every change, from a call here or a
//! file another program wrote, goes through [`changed`]: resolve once, carry it to
//! the desktop, publish the snapshot, announce the generation.

use crate::outputs;
use nyx_theme_core::{self as core, A11y, Registry, Scope, Suite, resolver, snapshot};
use std::sync::{Arc, Mutex};
use zbus::{Connection, fdo, object_server::SignalEmitter};

pub struct Inner {
    outputs: bool,
    last_suite: Option<Suite>,
    last_themes: Vec<String>,
    last_emitted: u64,
}

#[derive(Clone)]
pub struct State(Arc<Mutex<Inner>>);

impl State {
    pub fn new(outputs: bool) -> State {
        State(Arc::new(Mutex::new(Inner { outputs, last_suite: None, last_themes: vec![], last_emitted: 0 })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What identifies the installed themes, to notice one added, updated or removed.
fn themes_stamp(reg: &Registry) -> Vec<String> {
    reg.entries()
        .iter()
        .map(|e| {
            let modified = e
                .meta
                .path
                .as_ref()
                .and_then(|p| std::fs::metadata(if p.is_dir() { p.join(core::schema::MANIFEST) } else { p.clone() }).ok())
                .and_then(|m| m.modified().ok())
                .map(|t| format!("{t:?}"))
                .unwrap_or_default();
            format!("{}|{:?}|{}|{}", e.meta.id, e.meta.origin, e.meta.version.as_deref().unwrap_or(""), modified)
        })
        .collect()
}

fn write_outputs(r: &resolver::Resolved) {
    let appearance = nyx_config::Layers::standard().merged("appearance").unwrap_or_default();
    for (what, result) in outputs::write_all(&r.theme, &appearance) {
        if let Err(e) = result {
            eprintln!("nyx-themed: {what}: {e:#}");
        }
    }
}

/// Resolve and apply once, without the bus (`--once`).
pub fn refresh(outputs_on: bool, reason: &str) {
    let r = resolver::resolve(&Registry::load(), &core::suite(), Scope::System, &A11y::system());
    if outputs_on {
        write_outputs(&r);
    }
    match snapshot::publish(&r, reason, false) {
        Ok(p) => eprintln!("nyx-themed: {} applied (generation {})", r.meta.id, p.generation),
        Err(e) => eprintln!("nyx-themed: the snapshot couldn't be written: {e}"),
    }
}

/// Re-resolve after a change and announce it once. Overlapping calls are fine:
/// generations only go up and an older one is never announced after a newer.
pub async fn changed(conn: &Connection, state: &State, reason: &str) {
    let st = state.clone();
    let why = reason.to_string();
    let work = tokio::task::spawn_blocking(move || {
        let suite = core::suite();
        let reg = Registry::load();
        let stamp = themes_stamp(&reg);
        let mut inner = st.lock();
        let system_same = |old: &Suite| old.mode == suite.mode && old.theme == suite.theme && old.tokens == suite.tokens;
        let scope = match &inner.last_suite {
            Some(old) if system_same(old) && old.applications != suite.applications => "applications",
            Some(old) if system_same(old) && inner.last_themes != stamp => "themes",
            _ => "system",
        };
        let r = resolver::resolve(&reg, &suite, Scope::System, &A11y::system());
        for issue in r.issues.iter().filter(|i| i.severity == core::Severity::Error) {
            eprintln!("nyx-themed: {issue}");
        }
        if inner.outputs {
            write_outputs(&r);
        }
        let published = snapshot::publish(&r, &why, scope != "system");
        inner.last_suite = Some(suite);
        inner.last_themes = stamp;
        (published, r.meta.id.clone(), scope)
    })
    .await;
    let Ok((published, system_theme, scope)) = work else { return };
    let generation = match published {
        Ok(p) => p.generation,
        Err(e) => {
            eprintln!("nyx-themed: the snapshot couldn't be written: {e}");
            return;
        }
    };
    announce(conn, state, generation, &system_theme, scope, reason).await;
}

async fn announce(conn: &Connection, state: &State, generation: u64, system_theme: &str, scope: &str, reason: &str) {
    {
        let mut inner = state.lock();
        if generation <= inner.last_emitted {
            return;
        }
        inner.last_emitted = generation;
    }
    let Ok(emitter) = SignalEmitter::new(conn, core::dbus::PATH) else { return };
    if let Err(e) = Service::theme_changed(&emitter, generation, system_theme, scope, reason, core::SCHEMA_VERSION).await {
        eprintln!("nyx-themed: ThemeChanged: {e}");
    }
    eprintln!("nyx-themed: generation {generation}: {system_theme} ({scope}, {reason})");
}

fn failed(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> fdo::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(failed)?.map_err(failed)
}

pub struct Service {
    pub state: State,
}

#[zbus::interface(name = "io.github.design_nexus.nyx.Theme1")]
impl Service {
    /// Every theme on offer, as JSON metadata.
    async fn list_themes(&self) -> fdo::Result<String> {
        blocking(|| serde_json::to_string(&core::list_themes()).map_err(|e| e.to_string())).await
    }

    async fn get_theme_metadata(&self, theme_id: String) -> fdo::Result<String> {
        blocking(move || {
            let m = core::get_theme_metadata(&theme_id).ok_or(format!("there is no theme {theme_id}"))?;
            serde_json::to_string(&m).map_err(|e| e.to_string())
        })
        .await
    }

    /// The system theme's id (`omarchy` when following Omarchy's).
    async fn get_active_theme(&self) -> fdo::Result<String> {
        blocking(|| Ok(core::get_active_theme())).await
    }

    async fn get_generation(&self) -> u64 {
        snapshot::generation()
    }

    async fn get_snapshot_path(&self) -> String {
        core::paths::snapshot_file().display().to_string()
    }

    /// The theme `app_id` shows (empty for the system), as a snapshot in JSON.
    async fn resolve_effective_theme(&self, app_id: String) -> fdo::Result<String> {
        blocking(move || {
            let app = (!app_id.is_empty()).then_some(app_id.as_str());
            let r = core::resolve_effective_theme(app, &core::suite(), &A11y::system());
            serde_json::to_string(&snapshot::Snapshot::from_resolved(&r, snapshot::generation(), "resolve")).map_err(|e| e.to_string())
        })
        .await
    }

    async fn set_system_theme(&self, #[zbus(connection)] conn: &Connection, theme_id: String) -> fdo::Result<()> {
        blocking(move || core::set_system_theme(&theme_id)).await?;
        changed(conn, &self.state, "theme-selected").await;
        Ok(())
    }

    async fn set_app_override(&self, #[zbus(connection)] conn: &Connection, app_id: String, theme_id: String) -> fdo::Result<()> {
        blocking(move || core::set_app_override(&app_id, &theme_id)).await?;
        changed(conn, &self.state, "override-set").await;
        Ok(())
    }

    async fn clear_app_override(&self, #[zbus(connection)] conn: &Connection, app_id: String) -> fdo::Result<()> {
        blocking(move || core::clear_app_override(&app_id)).await?;
        changed(conn, &self.state, "override-cleared").await;
        Ok(())
    }

    /// Install every theme in a repository that passes its checks; their ids.
    async fn install_theme(&self, #[zbus(connection)] conn: &Connection, repository_url: String) -> fdo::Result<Vec<String>> {
        let ids = blocking(move || core::install_theme(&repository_url)).await?;
        changed(conn, &self.state, "theme-installed").await;
        Ok(ids)
    }

    /// `up-to-date`, or `updated <from> <to>`.
    async fn update_theme(&self, #[zbus(connection)] conn: &Connection, theme_id: String) -> fdo::Result<String> {
        let r = blocking(move || core::update_theme(&theme_id)).await?;
        changed(conn, &self.state, "theme-updated").await;
        Ok(match r {
            core::installer::Updated::UpToDate => "up-to-date".into(),
            core::installer::Updated::Updated { from, to } => format!("updated {from} {to}"),
        })
    }

    async fn remove_theme(&self, #[zbus(connection)] conn: &Connection, theme_id: String) -> fdo::Result<()> {
        blocking(move || core::remove_theme(&theme_id)).await?;
        changed(conn, &self.state, "theme-removed").await;
        Ok(())
    }

    /// Appearance back to the defaults.
    async fn reset(&self, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        blocking(core::reset_appearance).await?;
        changed(conn, &self.state, "reset").await;
        Ok(())
    }

    #[zbus(property)]
    async fn generation(&self) -> u64 {
        snapshot::generation()
    }

    #[zbus(signal)]
    pub async fn theme_changed(
        emitter: &SignalEmitter<'_>,
        generation: u64,
        system_theme: &str,
        scope: &str,
        reason: &str,
        schema: u32,
    ) -> zbus::Result<()>;
}

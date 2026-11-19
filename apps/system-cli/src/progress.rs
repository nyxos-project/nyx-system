//! What a long change looks like: its stages as they pass, a spinner with the latest
//! line the build printed, and a plain Done or Failed at the end, with a desktop
//! notification when it took a while. Everything the build printed goes to
//! `~/.local/state/nyx/system.log`; when a change fails, its last lines are shown too.
//! Without a terminal (scripts, Settings, `--json`), only the stages and notes are
//! printed, one a line.

use futures_util::StreamExt;
use nyx_system_proto::SystemProxy;
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Lines of the build kept to show when it fails.
const TAIL: usize = 20;
/// Changes that take longer than this end with a notification too.
const NOTIFY_AFTER: Duration = Duration::from_secs(20);

struct State {
    tty: bool,
    stage: Option<String>,
    /// The stage shown is only a placeholder (waiting for the password): replaced, not
    /// ticked, when the manager names its first stage.
    provisional: bool,
    last: String,
    tail: VecDeque<String>,
    log: Option<std::fs::File>,
    frame: usize,
}

impl State {
    fn clear(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
    }

    fn line(&mut self, line: &str) {
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{line}");
        }
        if let Some(stage) = line.strip_prefix("== ") {
            self.clear();
            if let Some(done) = self.stage.take().filter(|_| !self.provisional) {
                self.done(&done);
            }
            self.provisional = false;
            if !self.tty {
                eprintln!("• {stage}");
            }
            self.stage = Some(stage.to_string());
            self.last.clear();
        } else if is_note(line) {
            self.clear();
            eprintln!("  {}·{} {line}", dim(self.tty), reset(self.tty));
        } else if !line.trim().is_empty() && Some(line.trim_end_matches('…')) != self.stage.as_deref() {
            if self.tail.len() == TAIL {
                self.tail.pop_front();
            }
            self.tail.push_back(line.to_string());
            self.last = line.trim().to_string();
        }
    }

    fn done(&self, stage: &str) {
        if self.tty {
            eprintln!("  \x1b[32m✓\x1b[0m {stage}");
        }
    }

    fn draw(&mut self) {
        let Some(stage) = &self.stage else { return };
        if !self.tty {
            return;
        }
        self.frame = (self.frame + 1) % SPINNER.len();
        let room = width().saturating_sub(stage.chars().count() + 8);
        let last: String = self.last.chars().take(room).collect();
        eprint!("\r\x1b[2K  \x1b[36m{}\x1b[0m {stage}  \x1b[2m{last}\x1b[0m", SPINNER[self.frame]);
        let _ = std::io::stderr().flush();
    }
}

/// The manager's own notes (snapshots, what has to compile), shown as they come.
fn is_note(line: &str) -> bool {
    ["Snapshot ", "No snapshot", "The snapshot", "Compiling "].iter().any(|p| line.starts_with(p))
}

fn dim(tty: bool) -> &'static str {
    if tty { "\x1b[2m" } else { "" }
}

fn reset(tty: bool) -> &'static str {
    if tty { "\x1b[0m" } else { "" }
}

/// The terminal's width, or 80.
fn width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills a winsize; on failure it's left zeroed.
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

fn log_path() -> Option<std::path::PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))?;
    Some(state.join("nyx/system.log"))
}

fn open_log(title: &str) -> Option<std::fs::File> {
    let path = log_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let now = std::process::Command::new("date")
        .arg("+%F %T")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let _ = writeln!(f, "\n==== {now} {title}");
    Some(f)
}

fn took(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 { format!("{}m {}s", s / 60, s % 60) } else { format!("{s}s") }
}

/// A desktop notification through the session's notification server (Nyx Notify).
async fn notify(summary: &str, body: &str, ok: bool) {
    let Ok(conn) = zbus::Connection::session().await else { return };
    let icon = if ok { "emblem-ok-symbolic" } else { "dialog-error-symbolic" };
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value> = std::collections::HashMap::new();
    let _ = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("NyxOS", 0u32, icon, summary, body, Vec::<&str>::new(), hints, -1i32),
        )
        .await;
}

/// Run a long call on the manager, showing its progress under `title` ("Installing
/// NordVPN"); `done` says what happened when it worked ("NordVPN is installed").
pub async fn run<F, T>(p: &SystemProxy<'static>, title: &str, done: &str, call: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = zbus::Result<T>>,
{
    let tty = std::io::stderr().is_terminal();
    let started = Instant::now();
    if tty {
        eprintln!("\x1b[1m{title}\x1b[0m");
    } else {
        eprintln!("{title}");
    }
    let state = Arc::new(Mutex::new(State {
        tty,
        stage: Some("Waiting for permission".into()),
        provisional: true,
        last: String::new(),
        tail: VecDeque::new(),
        log: open_log(title),
        frame: 0,
    }));
    let mut lines = p.receive_progress().await?;
    let reader = {
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(signal) = lines.next().await {
                if let Ok(args) = signal.args() {
                    state.lock().unwrap().line(args.line());
                }
            }
        })
    };
    let spinner = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tick.tick().await;
                state.lock().unwrap().draw();
            }
        })
    };
    let result = call.await;
    // The last lines the manager sent may still be on their way.
    tokio::time::sleep(Duration::from_millis(150)).await;
    spinner.abort();
    reader.abort();
    let elapsed = started.elapsed();
    let st = state.lock().unwrap();
    st.clear();
    match &result {
        Ok(_) => {
            if let Some(stage) = st.stage.as_ref().filter(|_| !st.provisional) {
                st.done(stage);
            }
            if tty {
                eprintln!("\n\x1b[32m●\x1b[0m Done: {done} ({})", took(elapsed));
            } else {
                eprintln!("Done: {done} ({})", took(elapsed));
            }
        }
        Err(e) => {
            if let (true, Some(stage), false) = (tty, &st.stage, st.provisional) {
                eprintln!("  \x1b[31m✗\x1b[0m {stage}");
            }
            if !st.tail.is_empty() {
                eprintln!("\n{}The build's last lines:{}", dim(tty), reset(tty));
                for l in &st.tail {
                    eprintln!("  {}{l}{}", dim(tty), reset(tty));
                }
            }
            let why = e.to_string();
            let why = why.trim_start_matches("org.freedesktop.DBus.Error.Failed: ");
            if tty {
                eprintln!("\n\x1b[31m●\x1b[0m Failed: {why}");
            } else {
                eprintln!("Failed: {why}");
            }
            if let Some(path) = log_path() {
                eprintln!("  Everything it printed: {}", path.display());
            }
        }
    }
    drop(st);
    if elapsed >= NOTIFY_AFTER {
        match &result {
            Ok(_) => notify(done, &format!("{title}: done in {}", took(elapsed)), true).await,
            Err(e) => notify(&format!("{title}: failed"), &e.to_string(), false).await,
        }
    }
    result.map_err(|e| {
        let code = crate::exit::code(&anyhow::Error::from(e));
        anyhow::Error::from(crate::exit::Shown(code))
    })
}

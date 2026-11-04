//! The screens: a stack of lists, texts, questions and jobs. Esc goes back; a job shows
//! what it runs as it runs, and why it stopped if it didn't work.

use crate::accounts::{self, Person};
use crate::health;
use crate::paths::Paths;
use crate::restore::{self, Repo, Snapshot};
use crate::run::Log;
use crate::system;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

pub enum Exit {
    Quit,
    Reboot,
    PowerOff,
}

#[derive(Clone)]
enum Act {
    Generations,
    Repair,
    Health,
    Accounts,
    Restore,
    MakeDefault(u32),
    ShowSettings,
    Rebuild,
    ResetSettings,
    Errors,
    Failed(Vec<String>),
    Clean,
    Person(String),
    Password(String),
    Unlock(String),
    Repo(Repo),
    Drive(String),
    TypePath,
    Snapshot(PathBuf, String, Snapshot),
    RestoreInto(Box<(PathBuf, String, Snapshot, Person)>),
    Shell,
    Reboot,
    PowerOff,
    Quit,
    None,
}

struct Item {
    text: String,
    act: Act,
    warn: bool,
}

fn item(text: impl Into<String>, act: Act) -> Item {
    Item { text: text.into(), act, warn: false }
}

/// What a job hands back: words, or the next step of a restore.
enum Outcome {
    Message(String),
    AskPassword(PathBuf),
    Repos(Vec<PathBuf>),
    Snapshots(PathBuf, String, Vec<Snapshot>),
}

type Job = Box<dyn FnOnce(&Log) -> Result<Outcome> + Send>;

enum Then {
    Days,
    Password(String),
    PasswordAgain(String, String),
    RepoPassword(PathBuf),
    RepoPath,
    Include(Box<(PathBuf, String, Snapshot, Person)>),
}

enum Screen {
    List {
        title: String,
        note: String,
        items: Vec<Item>,
        state: ListState,
        source: Option<Act>,
    },
    Text {
        title: String,
        body: String,
        scroll: u16,
    },
    Confirm {
        title: String,
        text: String,
        job: Option<Job>,
    },
    Input {
        title: String,
        prompt: String,
        masked: bool,
        value: String,
        then: Then,
    },
    Work {
        title: String,
        lines: Vec<String>,
        rx: Receiver<String>,
        done: Receiver<Result<Outcome>>,
        result: Option<Result<String, String>>,
    },
}

struct App {
    paths: Paths,
    stack: Vec<Screen>,
    recovery_boot: bool,
    exit: Option<Exit>,
    shell: bool,
}

fn list(title: &str, note: &str, items: Vec<Item>, source: Option<Act>) -> Screen {
    let mut state = ListState::default();
    let first = items.iter().position(|i| !matches!(i.act, Act::None));
    state.select(first);
    Screen::List { title: title.into(), note: note.into(), items, state, source }
}

fn date(secs: i64) -> String {
    // Local time without a time-zone database: what `date` prints.
    crate::run::quiet(&["date", "-d", &format!("@{secs}"), "+%Y-%m-%d %H:%M"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

impl App {
    fn menu(&self) -> Screen {
        let mut items = vec![
            item("Generations: boot an earlier version of the system", Act::Generations),
            item("Repair: undo the last system change, rebuild, or reset the settings", Act::Repair),
            item("Health: disk space, what failed, the last boot's errors", Act::Health),
            item("Accounts: a new password, or unlock an account", Act::Accounts),
            item("Restore files from a Nyx Backup", Act::Restore),
            item("", Act::None),
            item("Open a root shell", Act::Shell),
            item("Restart", Act::Reboot),
            item("Power off", Act::PowerOff),
        ];
        if !self.recovery_boot {
            items.push(item("Quit", Act::Quit));
        }
        list("NyxOS Recovery", "Everything here works without the network. Changes take effect when you restart.", items, None)
    }

    fn build(&self, act: &Act) -> Option<Screen> {
        Some(match act {
            Act::Generations => {
                let gens = system::list(&self.paths, std::path::Path::new("/run/booted-system")).unwrap_or_default();
                let items = gens
                    .iter()
                    .map(|g| {
                        let mut t = format!("Generation {:>4}   {}   {}", g.number, date(g.created), g.version);
                        if g.default {
                            t.push_str("   · boots by default");
                        }
                        if g.running {
                            t.push_str("   · running now");
                        }
                        item(t, Act::MakeDefault(g.number))
                    })
                    .collect();
                list(
                    "Generations",
                    "Every system change is a generation. Choose one to boot from now on; its settings come back with it.",
                    items,
                    Some(Act::Generations),
                )
            }
            Act::Repair => {
                let gens = system::list(&self.paths, std::path::Path::new("/run/booted-system")).unwrap_or_default();
                let mut items = Vec::new();
                if let Some(p) = system::previous(&gens) {
                    items.push(item(format!("Undo the last system change (back to generation {p})"), Act::MakeDefault(p)));
                }
                items.push(item("Show the system settings", Act::ShowSettings));
                items.push(item("Rebuild the system from its settings (offline)", Act::Rebuild));
                items.push(item("Reset the system settings to the defaults, then rebuild", Act::ResetSettings));
                list(
                    "Repair",
                    "The system settings are what Settings, Welcome and the Store changed. A rebuild uses only what's on this computer.",
                    items,
                    Some(Act::Repair),
                )
            }
            Act::Health => {
                let mut items: Vec<Item> = health::spaces()
                    .into_iter()
                    .map(|s| Item {
                        text: format!(
                            "{:<6} {:>3}% used, {} free of {}{}",
                            s.mount,
                            s.used_percent(),
                            health::size(s.free),
                            health::size(s.total),
                            if s.low() { "   · nearly full" } else { "" }
                        ),
                        warn: s.low(),
                        act: Act::None,
                    })
                    .collect();
                items.push(item("", Act::None));
                let failed = health::failed_units();
                items.push(Item {
                    text: if failed.is_empty() {
                        "Nothing has failed since recovery started".into()
                    } else {
                        format!("{} things failed since recovery started", failed.len())
                    },
                    warn: !failed.is_empty(),
                    act: if failed.is_empty() { Act::None } else { Act::Failed(failed) },
                });
                items.push(item("Errors from the last boot", Act::Errors));
                items.push(item("Make room: remove old generations and what nothing uses", Act::Clean));
                list(
                    "Health",
                    "A full disk, especially /boot, is the usual reason a rebuild or an update fails.",
                    items,
                    Some(Act::Health),
                )
            }
            Act::Accounts => {
                let items = accounts::all(&self.paths)
                    .into_iter()
                    .map(|p| {
                        let t = format!("{} ({}){}", p.name, p.login, if p.locked { "   · locked" } else { "" });
                        Item { text: t, warn: p.locked, act: Act::Person(p.login) }
                    })
                    .collect();
                list("Accounts", "Choose someone to give a new password or to unlock.", items, Some(Act::Accounts))
            }
            Act::Person(login) => list(
                login,
                "",
                vec![
                    item("Set a new password", Act::Password(login.clone())),
                    item("Unlock: switch the password back on and forget failed tries", Act::Unlock(login.clone())),
                ],
                None,
            ),
            Act::Restore => {
                let mut items: Vec<Item> = restore::known(&accounts::all(&self.paths))
                    .into_iter()
                    .map(|r| item(format!("{}   {}", r.label, r.path.display()), Act::Repo(r)))
                    .collect();
                for d in restore::drives() {
                    let name = if d.label.is_empty() { d.device.clone() } else { format!("{} ({})", d.label, d.device) };
                    let at = d.mounted.as_deref().map(|m| format!(", at {m}")).unwrap_or_default();
                    items.push(item(format!("Look on {name}: {} {}{at}", d.fstype, health::size(d.size)), Act::Drive(d.device)));
                }
                items.push(item("Type the folder of a backup", Act::TypePath));
                list(
                    "Restore files",
                    "Files come back into a new folder in the person's home; nothing of theirs is replaced. Drives are opened read-only.",
                    items,
                    Some(Act::Restore),
                )
            }
            _ => return None,
        })
    }

    fn work(&mut self, title: &str, job: Job) {
        let (tx, rx) = mpsc::channel();
        let (dtx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let r = job(&tx);
            let _ = dtx.send(r);
        });
        self.stack.push(Screen::Work { title: title.into(), lines: Vec::new(), rx, done, result: None });
    }

    fn confirm(&mut self, title: &str, text: String, job: Job) {
        self.stack.push(Screen::Confirm { title: title.into(), text, job: Some(job) });
    }

    fn input(&mut self, title: &str, prompt: &str, masked: bool, then: Then) {
        self.stack.push(Screen::Input { title: title.into(), prompt: prompt.into(), masked, value: String::new(), then });
    }

    fn act(&mut self, act: Act) {
        if let Some(s) = self.build(&act) {
            self.stack.push(s);
            return;
        }
        let p = self.paths.clone();
        match act {
            Act::MakeDefault(n) => self.confirm(
                "Boot an earlier generation",
                format!(
                    "Make generation {n} the one that boots from now on?\n\nThe system settings and versions it was built from are put back, so changes you make next build on it. The current ones are kept beside them (.before-recovery)."
                ),
                Box::new(move |log| system::make_default(&p, n, log).map(Outcome::Message)),
            ),
            Act::ShowSettings => {
                self.stack.push(Screen::Text { title: "System settings".into(), body: system::settings_now(&self.paths), scroll: 0 })
            }
            Act::Rebuild => self.confirm(
                "Rebuild",
                "Build the system from its settings with only what's on this computer, and boot it from now on?".into(),
                Box::new(move |log| system::rebuild_offline(&p, log).map(Outcome::Message)),
            ),
            Act::ResetSettings => self.confirm(
                "Reset the system settings",
                "Remove every system change made in Settings, Welcome and the Store (software for everyone, services, people, mounts…), then rebuild?\n\nThe old settings are kept beside them (system.json.before-recovery).".into(),
                Box::new(move |log| system::reset_settings(&p, log).map(Outcome::Message)),
            ),
            Act::Errors => self.stack.push(Screen::Text { title: "The last boot's errors".into(), body: health::last_boot_errors(), scroll: 0 }),
            Act::Failed(units) => self.stack.push(Screen::Text { title: "What failed".into(), body: units.join("\n"), scroll: 0 }),
            Act::Clean => self.input("Make room", "Remove generations older than how many days? (the default one always stays)", false, Then::Days),
            Act::Password(login) => self.input("New password", &format!("A new password for {login}"), true, Then::Password(login)),
            Act::Unlock(login) => self.confirm(
                "Unlock",
                format!("Switch {login}'s password back on and forget their failed tries?"),
                Box::new(move |log| accounts::unlock(&login, log).map(Outcome::Message)),
            ),
            Act::Repo(r) => match r.drive {
                Some((source, inner)) => self.work(
                    "Opening the drive",
                    Box::new(move |log| {
                        let at = restore::mount(&p, &source, log)?.join(inner);
                        anyhow::ensure!(restore::is_repo(&at), "there's no backup at {} on that drive", at.display());
                        Ok(Outcome::AskPassword(at))
                    }),
                ),
                None => self.input("Backup password", &format!("The password of the backup at {}", r.path.display()), true, Then::RepoPassword(r.path)),
            },
            Act::Drive(device) => self.work(
                "Looking on the drive",
                Box::new(move |log| {
                    let at = restore::mount(&p, &device, log)?;
                    let found = restore::find(&at, 4);
                    anyhow::ensure!(!found.is_empty(), "no Nyx Backup on that drive (looked four folders deep in {})", at.display());
                    Ok(Outcome::Repos(found))
                }),
            ),
            Act::TypePath => self.input("A backup's folder", "The folder of a Nyx Backup (restic) repository", false, Then::RepoPath),
            Act::Snapshot(repo, pw, snap) => {
                let items = accounts::all(&self.paths)
                    .into_iter()
                    .map(|person| item(format!("Into {}'s home ({})", person.name, person.home), Act::RestoreInto(Box::new((repo.clone(), pw.clone(), snap.clone(), person)))))
                    .collect();
                self.stack.push(list("Whose home", &format!("Backup from {}: {}", snap.when(), snap.paths.join(", ")), items, None));
            }
            Act::RestoreInto(choice) => {
                let home = choice.3.home.clone();
                self.input(
                    "What to restore",
                    &format!("A folder or file in the backup to restore (like {home}/Documents), or leave it empty for everything"),
                    false,
                    Then::Include(choice),
                )
            }
            Act::Shell => self.shell = true,
            Act::Reboot => self.exit = Some(Exit::Reboot),
            Act::PowerOff => self.exit = Some(Exit::PowerOff),
            Act::Quit => self.exit = Some(Exit::Quit),
            _ => {}
        }
    }

    fn submit(&mut self, then: Then, value: String) {
        let p = self.paths.clone();
        match then {
            Then::Days => match value.trim().parse::<u32>() {
                Ok(days) => self.confirm(
                    "Make room",
                    format!("Remove generations older than {days} days, and everything only they used?\n\nThey can't be booted afterwards."),
                    Box::new(move |log| health::clean(&p, days, log).map(Outcome::Message)),
                ),
                Err(_) => self.stack.push(Screen::Text { title: "Make room".into(), body: "That isn't a number of days.".into(), scroll: 0 }),
            },
            Then::Password(login) => self.input("New password", &format!("The same password again, for {login}"), true, Then::PasswordAgain(login, value)),
            Then::PasswordAgain(login, first) => {
                if first != value {
                    self.stack.push(Screen::Text { title: "New password".into(), body: "The two passwords weren't the same; nothing changed.".into(), scroll: 0 });
                } else {
                    self.work("New password", Box::new(move |log| accounts::set_password(&login, &value, log).map(Outcome::Message)));
                }
            }
            Then::RepoPath => {
                let path = PathBuf::from(value.trim());
                if restore::is_repo(&path) {
                    self.input("Backup password", &format!("The password of the backup at {}", path.display()), true, Then::RepoPassword(path));
                } else {
                    self.stack.push(Screen::Text { title: "Restore files".into(), body: format!("{} isn't a Nyx Backup repository.", path.display()), scroll: 0 });
                }
            }
            Then::RepoPassword(path) => self.work(
                "Opening the backup",
                Box::new(move |log| {
                    let list = restore::snapshots(&path, &value, log)?;
                    anyhow::ensure!(!list.is_empty(), "this backup has nothing in it yet");
                    Ok(Outcome::Snapshots(path, value, list))
                }),
            ),
            Then::Include(choice) => {
                let (repo, pw, snap, person) = *choice;
                let only = value.trim().to_string();
                let what = if only.is_empty() { "everything in it".to_string() } else { only.clone() };
                let into = restore::target_for(&person, &snap);
                self.confirm(
                    "Restore",
                    format!("Restore {what} from the backup of {} into\n{}?\n\nNothing already there is replaced.", snap.when(), into.display()),
                    Box::new(move |log| {
                        restore::restore(&repo, &pw, &snap, (!only.is_empty()).then_some(only.as_str()), &person, log).map(Outcome::Message)
                    }),
                );
            }
        }
    }

    /// A finished job's next step, or its words.
    fn finished(&mut self, outcome: Result<Outcome>) -> Option<Result<String, String>> {
        match outcome {
            Err(e) => Some(Err(format!("{e:#}"))),
            Ok(Outcome::Message(m)) => Some(Ok(m)),
            Ok(Outcome::AskPassword(path)) => {
                self.stack.pop();
                self.input(
                    "Backup password",
                    &format!("The password of the backup at {}", path.display()),
                    true,
                    Then::RepoPassword(path),
                );
                None
            }
            Ok(Outcome::Repos(found)) => {
                self.stack.pop();
                let items = found
                    .into_iter()
                    .map(|path| item(path.display().to_string(), Act::Repo(Repo { label: String::new(), path, drive: None })))
                    .collect();
                self.stack.push(list("Backups on the drive", "", items, None));
                None
            }
            Ok(Outcome::Snapshots(repo, pw, snaps)) => {
                self.stack.pop();
                let items = snaps
                    .into_iter()
                    .map(|s| {
                        let t = format!("{}   {}   {}", s.when(), s.hostname, s.paths.join(", "));
                        item(t, Act::Snapshot(repo.clone(), pw.clone(), s))
                    })
                    .collect();
                self.stack.push(list("Backups", "Choose the one to restore from (newest first).", items, None));
                None
            }
        }
    }

    /// Back from a job or a question: the list under it shows what changed.
    fn pop(&mut self) {
        self.stack.pop();
        while matches!(self.stack.last(), Some(Screen::Confirm { job: None, .. })) {
            self.stack.pop();
        }
        let refresh = match self.stack.last() {
            Some(Screen::List { source: Some(src), state, .. }) => Some((src.clone(), state.selected())),
            _ => None,
        };
        if let Some((src, sel)) = refresh
            && let Some(mut s) = self.build(&src)
        {
            if let Screen::List { state, items, .. } = &mut s {
                state.select(sel.filter(|&i| i < items.len()).or(state.selected()));
            }
            if let Some(top) = self.stack.last_mut() {
                *top = s;
            }
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        enum Do {
            Act(Act),
            Pop,
            Work(String, Job),
            Submit,
            Nothing,
        }
        let root = self.stack.len() <= 1;
        let d = match self.stack.last_mut() {
            None => Do::Nothing,
            Some(Screen::List { items, state, .. }) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    move_sel(state, items, -1);
                    Do::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_sel(state, items, 1);
                    Do::Nothing
                }
                KeyCode::Enter => state.selected().and_then(|i| items.get(i)).map_or(Do::Nothing, |i| Do::Act(i.act.clone())),
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left if !root => Do::Pop,
                _ => Do::Nothing,
            },
            Some(Screen::Text { scroll, .. }) => match code {
                KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                    Do::Nothing
                }
                KeyCode::Down => {
                    *scroll = scroll.saturating_add(1);
                    Do::Nothing
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(20);
                    Do::Nothing
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    *scroll = scroll.saturating_add(20);
                    Do::Nothing
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace | KeyCode::Left => Do::Pop,
                _ => Do::Nothing,
            },
            Some(Screen::Confirm { job, title, .. }) => match code {
                KeyCode::Char('y') | KeyCode::Enter => job.take().map_or(Do::Nothing, |j| Do::Work(title.clone(), j)),
                KeyCode::Char('n') | KeyCode::Esc => Do::Pop,
                _ => Do::Nothing,
            },
            Some(Screen::Input { value, .. }) => match code {
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    value.push(c);
                    Do::Nothing
                }
                KeyCode::Backspace => {
                    value.pop();
                    Do::Nothing
                }
                KeyCode::Esc => Do::Pop,
                KeyCode::Enter => Do::Submit,
                _ => Do::Nothing,
            },
            Some(Screen::Work { result: Some(_), .. }) if matches!(code, KeyCode::Esc | KeyCode::Enter) => Do::Pop,
            Some(Screen::Work { .. }) => Do::Nothing,
        };
        match d {
            Do::Act(a) => self.act(a),
            Do::Pop => self.pop(),
            Do::Work(title, job) => self.work(&title, job),
            Do::Submit => {
                if let Some(Screen::Input { then, value, .. }) = self.stack.pop() {
                    self.submit(then, value);
                }
            }
            Do::Nothing => {}
        }
    }

    /// Take a running job's lines, and its end.
    fn poll(&mut self) {
        let mut finished = None;
        if let Some(Screen::Work { lines, rx, done, result, .. }) = self.stack.last_mut() {
            while let Ok(l) = rx.try_recv() {
                lines.push(l);
            }
            if result.is_none()
                && let Ok(r) = done.try_recv()
            {
                while let Ok(l) = rx.try_recv() {
                    lines.push(l);
                }
                finished = Some(r);
            }
        }
        if let Some(r) = finished {
            let words = self.finished(r);
            if let (Some(w), Some(Screen::Work { result, .. })) = (words, self.stack.last_mut()) {
                *result = Some(w);
            }
        }
    }
}

fn move_sel(state: &mut ListState, items: &[Item], by: i32) {
    let Some(mut i) = state.selected() else { return };
    loop {
        let next = i as i32 + by;
        if next < 0 || next >= items.len() as i32 {
            return;
        }
        i = next as usize;
        if !matches!(items[i].act, Act::None) {
            state.select(Some(i));
            return;
        }
    }
}

fn header(app: &App) -> Line<'static> {
    let running = system::list(&app.paths, std::path::Path::new("/run/booted-system"))
        .ok()
        .and_then(|l| l.into_iter().find(|g| g.running))
        .map(|g| format!("generation {} running", g.number))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(" NyxOS Recovery ", Style::new().bold().fg(Color::Black).bg(Color::LightBlue)),
        Span::raw(format!("  {running}{}", if app.recovery_boot { " · recovery boot, offline" } else { "" })),
    ])
}

fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    f.render_widget(header(app), top);
    let help = match app.stack.last() {
        Some(Screen::List { .. }) => "↑↓ choose · Enter open · Esc back",
        Some(Screen::Text { .. }) => "↑↓ PgUp PgDn scroll · Esc back",
        Some(Screen::Confirm { .. }) => "y or Enter: yes · n or Esc: no",
        Some(Screen::Input { .. }) => "Enter: done · Esc: cancel",
        Some(Screen::Work { result: None, .. }) => "Working… (this can take a while)",
        Some(Screen::Work { .. }) => "Enter or Esc: back",
        None => "",
    };
    f.render_widget(Line::from(Span::styled(format!(" {help}"), Style::new().dim())), foot);
    let block =
        |t: &str| Block::default().borders(Borders::ALL).title(format!(" {t} ")).border_style(Style::new().fg(Color::LightBlue));
    match app.stack.last_mut() {
        Some(Screen::List { title, note, items, state, .. }) => {
            let [n, l] =
                Layout::vertical([Constraint::Length(if note.is_empty() { 0 } else { 3 }), Constraint::Min(1)]).areas(body);
            f.render_widget(
                Paragraph::new(note.as_str())
                    .wrap(Wrap { trim: true })
                    .block(Block::default().padding(ratatui::widgets::Padding::horizontal(1))),
                n,
            );
            let rows: Vec<ListItem> = items
                .iter()
                .map(|i| {
                    let style = if i.warn {
                        Style::new().fg(Color::LightRed)
                    } else if matches!(i.act, Act::None) {
                        Style::new().dim()
                    } else {
                        Style::new()
                    };
                    ListItem::new(i.text.clone()).style(style)
                })
                .collect();
            let widget = List::new(rows).block(block(title)).highlight_style(Style::new().reversed()).highlight_symbol("› ");
            f.render_stateful_widget(widget, l, state);
        }
        Some(Screen::Text { title, body: text, scroll }) => {
            f.render_widget(
                Paragraph::new(text.as_str()).wrap(Wrap { trim: false }).scroll((*scroll, 0)).block(block(title)),
                body,
            );
        }
        Some(Screen::Confirm { title, text, .. }) => {
            let area = centered(body, 70, 12);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(text.as_str()).wrap(Wrap { trim: true }).block(block(title)), area);
        }
        Some(Screen::Input { title, prompt, masked, value, .. }) => {
            let area = centered(body, 70, 7);
            f.render_widget(Clear, area);
            let shown = if *masked { "•".repeat(value.chars().count()) } else { value.clone() };
            let text = vec![
                Line::from(prompt.clone()),
                Line::from(""),
                Line::from(Span::styled(format!("{shown}▏"), Style::new().bold())),
            ];
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block(title)), area);
        }
        Some(Screen::Work { title, lines, result, .. }) => {
            let [l, r] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(if result.is_some() { 4 } else { 0 })]).areas(body);
            let height = l.height.saturating_sub(2) as usize;
            let tail: Vec<Line> =
                lines.iter().skip(lines.len().saturating_sub(height)).map(|s| Line::from(s.clone()).dim()).collect();
            f.render_widget(Paragraph::new(tail).block(block(title)), l);
            if let Some(res) = result {
                let (text, color) = match res {
                    Ok(m) => (m.clone(), Color::LightGreen),
                    Err(e) => (format!("That didn't work: {e}"), Color::LightRed),
                };
                f.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: true })
                        .style(Style::new().fg(color))
                        .block(Block::default().borders(Borders::TOP)),
                    r,
                );
            }
        }
        None => {}
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn shell(terminal: &mut DefaultTerminal) {
    ratatui::restore();
    println!("A root shell. Type exit to go back to NyxOS Recovery.");
    let _ = std::process::Command::new("bash").arg("--login").status();
    *terminal = ratatui::init();
}

pub fn run(paths: Paths) -> Result<Exit> {
    let recovery_boot =
        std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.split_whitespace().any(|w| w == "nyx.recovery=1"));
    let mut app = App { paths, stack: Vec::new(), recovery_boot, exit: None, shell: false };
    let menu = app.menu();
    app.stack.push(menu);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<Exit> {
        loop {
            app.poll();
            terminal.draw(|f| draw(f, &mut app))?;
            if let Some(e) = app.exit.take() {
                return Ok(e);
            }
            if app.shell {
                app.shell = false;
                shell(&mut terminal);
                continue;
            }
            if event::poll(Duration::from_millis(150))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) && !app.recovery_boot {
                    return Ok(Exit::Quit);
                }
                app.key(k.code, k.modifiers);
            }
        }
    })();
    ratatui::restore();
    result
}

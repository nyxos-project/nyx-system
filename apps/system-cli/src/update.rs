//! `nyx-system update` (and `nyx update`, which hands off to it): the plan, asked about
//! once, then carried out with its progress; `check` for what's available, `status` for
//! everything installed and its state, `auto` for the daily run, and `history` for what
//! was done. `--json` gives Settings and the Store the same plan and outcome
//! (`--yes --json`: no questions, the outcome on stdout, progress on stderr; a step that
//! failed is in the outcome, and the exit code stays 0).

use crate::exit;
use anyhow::{Result, bail};
use nyx_system_proto::HistoryEntry;
use nyx_update::cache::{self, Checked};
use nyx_update::policy::{self, Network, Policy};
use nyx_update::run::Outcome;
use nyx_update::{Ambiguous, Category, Item, Pick, Plan, Snapshot, Step, Target, Unknown};

pub const USAGE: &str = "  nyx-system update [TARGET...] [--dry-run] [--yes] [--json]
                                          Update everything, or: system (NyxOS and the
                                          Nyx apps built into it), apps (your apps,
                                          Flatpaks), tools (coding agents, your mise
                                          and uv tools), firmware, or one thing by
                                          name (claude, codex, firefox...). Plans
                                          first and asks; --dry-run only plans
  nyx-system update check [--json]        What has updates (the last check's, offline)
  nyx-system update status [--notify] [--json]
                                          Everything installed: its version, what's
                                          newer, how it updates and its update setting
  nyx-system update auto [--now]          The daily run: installs what the update
                                          settings say to, notifies about the rest
  nyx-system update history [N] [--json]  What was updated, newest first
  nyx-system update report [N] [--json]   How the last N (200) updates went: failures,
                                          rollbacks, restarts, typical times";

#[derive(Default)]
struct Flags {
    dry_run: bool,
    yes: bool,
    json: bool,
    notify: bool,
    now: bool,
}

fn flags(args: &[&str]) -> Result<(Flags, Vec<String>)> {
    let mut f = Flags::default();
    let mut names = Vec::new();
    for a in args {
        match *a {
            "--dry-run" | "-n" => f.dry_run = true,
            "--yes" | "-y" => f.yes = true,
            "--json" => f.json = true,
            "--notify" => f.notify = true,
            "--now" => f.now = true,
            // `nix profile upgrade --all`, as `nyx update` used to be.
            "--all" => {}
            a if a.starts_with('-') => return Err(exit::fail(exit::USAGE, format!("unknown option {a}; see nyx-system --help"))),
            a => names.push(a.to_string()),
        }
    }
    Ok((f, names))
}

use exit::confirm;

pub async fn run(args: &[&str]) -> Result<()> {
    match args {
        [.., "--help" | "-h"] | ["help"] => {
            println!("{}", USAGE.replace("  nyx-system update", "  nyx update"));
            Ok(())
        }
        ["status", rest @ ..] => status(rest, false).await,
        ["check", rest @ ..] => status(rest, true).await,
        ["auto", rest @ ..] => auto(rest).await,
        ["history", rest @ ..] => history(rest).await,
        ["report", rest @ ..] => report(rest).await,
        _ => update(args).await,
    }
}

/// What each name stands for, asking which one a name means when it fits several and
/// someone can be asked; an unknown or ambiguous name otherwise exits with
/// [`exit::NAME`].
fn picks(s: &Snapshot, targets: &[Target], names: &[String], ask: bool) -> Result<Vec<Pick>> {
    if names.is_empty() {
        return nyx_update::resolve_installed(s, targets, &[]);
    }
    let mut out = Vec::new();
    for name in names {
        match nyx_update::resolve_installed(s, targets, &[name.as_str()]) {
            Ok(p) => out.extend(p),
            Err(e) => {
                if let Some(a) = e.downcast_ref::<Ambiguous>() {
                    let i = exit::choose(name, &a.described, ask)?;
                    out.push(Pick::Target(a.options[i]));
                } else if let Some(u) = e.downcast_ref::<Unknown>() {
                    return Err(exit::fail(exit::NAME, u.to_string()));
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(out)
}

/// The probe's result kept for later (offline, Settings' "last checked").
fn remember(s: &Snapshot, targets: &[Target]) -> Checked {
    let checked = Checked {
        time: cache::now(),
        status: nyx_update::status(s, targets),
        targets: targets.to_vec(),
        firmware: s.firmware.clone(),
    };
    cache::save(&checked);
    checked
}

fn wants_firmware(names: &[String]) -> bool {
    names.is_empty() || names.iter().any(|n| n == "firmware" || n.starts_with("firmware:"))
}

fn print_items(items: &[Item]) {
    let width = items.iter().map(|i| i.name.chars().count()).max().unwrap_or(0).min(32);
    for i in items {
        let change = match (&i.from, &i.to) {
            (Some(f), Some(t)) => format!("{f} → {t}"),
            (None, Some(t)) => format!("→ {t}"),
            (Some(f), None) => f.clone(),
            (None, None) => String::new(),
        };
        if change.is_empty() {
            println!("  {:<width$}  {}", i.name, i.how);
        } else {
            println!("  {:<width$}  {change}  ({})", i.name, i.how);
        }
    }
}

fn print_plan(plan: &Plan) {
    if plan.items.is_empty() {
        println!("Nothing to update.");
    } else {
        println!("To update:");
        print_items(&plan.items);
    }
    for n in &plan.notes {
        println!("• {n}");
    }
    if plan.admin {
        println!("The system is rebuilt, which asks for the administrator's password. It's switched to only if it builds.");
    }
    if plan.may_restart {
        println!("A restart of the computer may be needed afterwards.");
    }
}

async fn update(args: &[&str]) -> Result<()> {
    let (f, names) = flags(args)?;
    let request = if names.is_empty() { "nyx update".to_string() } else { format!("nyx update {}", names.join(" ")) };
    match policy::network().await {
        Network::Offline => {
            return Err(exit::fail(exit::OFFLINE, "there's no network connection, so nothing can be checked or downloaded"));
        }
        Network::Metered if !f.json => eprintln!("This is a metered connection; updates can download a lot."),
        _ => {}
    }
    let settings = policy::Settings::load();
    if !f.json {
        eprintln!("Checking what's new…");
    }
    let started = std::time::Instant::now();
    let snapshot = nyx_update::probe::probe(nyx_update::probe::Options {
        check: true,
        firmware: (settings.firmware && wants_firmware(&names)) || names.iter().any(|n| n == "firmware"),
    });
    let check_seconds = started.elapsed().as_secs();
    let targets = nyx_update::targets(&snapshot);
    remember(&snapshot, &targets);
    let picks = picks(&snapshot, &targets, &names, !f.yes && !f.json)?;
    let mut plan = nyx_update::plan(&snapshot, &targets, &picks, &request);
    plan.check_seconds = check_seconds;

    if f.json && f.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    if !f.json {
        print_plan(&plan);
    }
    if f.dry_run {
        return Ok(());
    }
    if plan.suggest_system && !f.yes && confirm("Update the system instead?")? {
        plan = system_plan(&snapshot, &targets, &request);
        plan.check_seconds = check_seconds;
        print_plan(&plan);
    }
    if plan.is_empty() {
        if f.json {
            println!("{}", serde_json::to_string_pretty(&Outcome::default())?);
        }
        return Ok(());
    }

    // Firmware only when the person agrees to it: by asking for it, or when asked.
    let has_firmware = plan.steps.iter().any(|s| matches!(s, Step::Firmware { .. }));
    let mut firmware = has_firmware && !plan.firmware_implied && f.yes;
    if !f.yes {
        if !confirm("Update?")? {
            return Err(exit::fail(exit::DECLINED, "nothing was changed"));
        }
        if has_firmware {
            firmware =
                confirm("Update the firmware too? Firmware can't be undone, and the computer must stay on and plugged in.")?;
        }
    } else if has_firmware && plan.firmware_implied && !f.json {
        println!("• Firmware is left alone: run nyx update firmware to update it.");
    }

    let json = f.json;
    let mut line = |l: &str| {
        if let Some(stage) = l.strip_prefix("== ") {
            eprintln!("==> {stage}");
        } else if !json {
            eprintln!("    {l}");
        } else {
            eprintln!("{l}");
        }
    };
    let outcome = nyx_update::run::execute(&plan, firmware, &mut line).await?;
    // With --json, what failed is in the outcome (Settings and the Store read it).
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        return Ok(());
    }
    print_outcome(&outcome);
    if !outcome.ok() {
        let some = outcome.steps.iter().any(|s| s.ok);
        std::process::exit(if some { exit::PARTIAL } else { exit::FAILED });
    }
    Ok(())
}

/// The plan for the whole channel, when something asked for comes with it.
fn system_plan(s: &Snapshot, targets: &[Target], request: &str) -> Plan {
    nyx_update::plan(s, targets, &[Pick::Category(Category::System)], request)
}

fn print_outcome(o: &Outcome) {
    for s in &o.steps {
        println!("{} {}", if s.ok { "✓" } else { "✗" }, s.message);
        if let Some(r) = &s.system {
            for c in r.changes.iter().take(15) {
                println!("    {}: {} → {}", c.name, c.from, c.to);
            }
            if r.changes.len() > 15 {
                println!("    and {} more (nyx update history)", r.changes.len() - 15);
            }
        }
    }
    if o.steps.iter().any(|s| s.ok) && o.steps.iter().any(|s| !s.ok) {
        println!("Some of it was updated and some wasn't; what failed was left as it was.");
    }
    if !o.restart.is_empty() {
        println!("Reopen these to use their new versions: {}", o.restart.join(", "));
    }
    if o.reboot {
        println!("Restart the computer to finish the update.");
    }
}

/// `status` (everything installed) or `check` (what has updates), with `--json` for
/// Settings. Offline, both show the last check's and say so.
async fn status(args: &[&str], check: bool) -> Result<()> {
    let (f, names) = flags(args)?;
    if !names.is_empty() {
        return Err(exit::fail(exit::USAGE, "nyx-system update status and check take no names"));
    }
    let settings = policy::Settings::load();
    if f.notify && !settings.check {
        return Ok(());
    }
    let net = policy::network().await;
    if let Some(why) = policy::deferred(&settings, net).filter(|_| f.notify) {
        println!("Not checking now: {why}.");
        return Ok(());
    }
    let offline = net == Network::Offline;
    let checked = if offline {
        match cache::load() {
            Some(c) => c,
            None => return Err(exit::fail(exit::OFFLINE, "there's no network connection, and nothing has been checked yet")),
        }
    } else {
        let snapshot = nyx_update::probe::probe(nyx_update::probe::Options { check: true, firmware: settings.firmware });
        let targets = nyx_update::targets(&snapshot);
        remember(&snapshot, &targets)
    };
    if f.json {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Out<'a> {
            status: &'a nyx_update::Status,
            targets: &'a [Target],
            firmware: &'a nyx_update::Firmware,
            /// When this was checked (seconds since the epoch).
            last_checked: i64,
            /// There's no network: this is the last check's.
            offline: bool,
            /// Each kind's update setting: off, check, notify or auto.
            policies: std::collections::BTreeMap<Category, Policy>,
            settings: &'a policy::Settings,
        }
        let policies = Category::ALL.into_iter().map(|c| (c, settings.policy(c))).collect();
        let out = Out {
            status: &checked.status,
            targets: &checked.targets,
            firmware: &checked.firmware,
            last_checked: checked.time,
            offline,
            policies,
            settings: &settings,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if offline {
            println!("There's no network connection; this is what the check on {} found.", nyx_update::date(checked.time));
        }
        if check {
            if checked.status.available.is_empty() {
                println!("Everything is up to date.");
            } else {
                println!("Updates available:");
                print_items(&checked.status.available);
            }
        } else {
            print_status(&checked, &settings);
        }
        for n in &checked.status.notes {
            println!("• {n}");
        }
    }
    if f.notify && settings.notify && !offline {
        nyx_update::run::notify_available(&checked.status).await?;
    }
    Ok(())
}

/// Everything installed, by kind: its version, what's newer, how it updates.
fn print_status(c: &Checked, settings: &policy::Settings) {
    for cat in Category::ALL {
        let list: Vec<&Target> = c.targets.iter().filter(|t| t.category == cat).collect();
        let policy = match settings.policy(cat) {
            Policy::Off => "not checked",
            Policy::Check => "checked, no notification",
            Policy::Notify => "notifies; you install",
            Policy::Auto => "installed automatically",
        };
        println!("{} ({policy})", cat.title());
        if list.is_empty() {
            match (cat, &c.firmware) {
                (Category::Firmware, nyx_update::Firmware::Unavailable { reason }) => println!("  {reason}"),
                (Category::Firmware, nyx_update::Firmware::NotChecked) => println!("  not checked"),
                _ => println!("  nothing with updates here"),
            }
            continue;
        }
        // Apps that come with the system update with it: one line, not one each.
        let with_system: Vec<&str> = list
            .iter()
            .filter(|t| t.provider == nyx_update::Provider::Channel && cat != Category::System)
            .map(|t| t.name.as_str())
            .collect();
        let list: Vec<&Target> =
            list.into_iter().filter(|t| t.provider != nyx_update::Provider::Channel || cat == Category::System).collect();
        if !with_system.is_empty() {
            println!(
                "  {} app{} built into the system, updated with it",
                with_system.len(),
                if with_system.len() == 1 { "" } else { "s" }
            );
        }
        let width = list.iter().map(|t| t.name.chars().count()).max().unwrap_or(0).min(32);
        for t in list {
            let state = match (&t.installed, &t.available) {
                (Some(i), Some(a)) => format!("{i} → {a}"),
                (None, Some(a)) => format!("→ {a}"),
                (Some(i), None) if t.checkable => format!("{i}, up to date"),
                (Some(i), None) => format!("{i}, checked when it updates"),
                (None, None) => "with the system".into(),
            };
            println!("  {:<width$}  {state}  ({})", t.name, nyx_update::how(t));
        }
    }
    println!("Checked {}.", nyx_update::date(c.time));
}

/// The daily run (the nyx-update-check timer): the person's own software in the
/// categories set to `auto` is installed (at `install-at`, if set), what's set to
/// `notify` is announced, and the system and firmware are never installed by it.
async fn auto(args: &[&str]) -> Result<()> {
    let (f, _) = flags(args)?;
    let settings = policy::Settings::load();
    if !settings.check {
        return Ok(());
    }
    let net = policy::network().await;
    if let Some(why) = policy::deferred(&settings, net) {
        println!("Not checking now: {why}.");
        return Ok(());
    }
    let watched: Vec<Category> = Category::ALL.into_iter().filter(|c| settings.policy(*c) != Policy::Off).collect();
    let snapshot =
        nyx_update::probe::probe(nyx_update::probe::Options { check: true, firmware: watched.contains(&Category::Firmware) });
    let targets = nyx_update::targets(&snapshot);
    remember(&snapshot, &targets);

    let auto = settings.with(Policy::Auto);
    let picks = nyx_update::unattended(&snapshot, &targets, &auto);
    let mut installed: Vec<usize> = Vec::new();
    if !picks.is_empty() {
        let due = f.now || settings.install_at.is_none_or(|at| policy::within(at, local_time()));
        if due {
            let plan = nyx_update::plan(&snapshot, &targets, &picks, "automatic update");
            println!("Updating {} in the background.", plan.items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", "));
            let outcome = nyx_update::run::execute(&plan, false, &mut |l| eprintln!("{l}")).await?;
            if outcome.ok() {
                installed = picks.iter().filter_map(|p| if let Pick::Target(i) = p { Some(*i) } else { None }).collect();
                if settings.notify && !plan.items.is_empty() {
                    let names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).take(5).collect();
                    let reopen = if outcome.restart.is_empty() {
                        String::new()
                    } else {
                        format!("\nReopen {} to use the new versions.", outcome.restart.join(", "))
                    };
                    let _ = nyx_update::run::notify("Updated in the background", &format!("{}{reopen}", names.join(", "))).await;
                }
            } else {
                let why: Vec<String> = outcome.steps.iter().filter(|s| !s.ok).map(|s| s.message.clone()).collect();
                let _ = nyx_update::run::notify(
                    "An automatic update failed",
                    &format!("{}\nWhat failed was left as it was. Settings > Updates has the details.", why.join("; ")),
                )
                .await;
            }
        } else if let Some((h, m)) = settings.install_at {
            schedule(h, m);
            println!("Automatic updates wait for {h:02}:{m:02}.");
        }
    }

    // Announce the rest of what's set to notify (or to auto, but only installable by hand).
    let st = nyx_update::Status {
        available: targets
            .iter()
            .enumerate()
            .filter(|(i, t)| t.has_update() && !installed.contains(i))
            .filter(|(_, t)| matches!(settings.policy(t.category), Policy::Notify | Policy::Auto))
            .map(|(_, t)| Item {
                name: t.name.clone(),
                from: t.installed.clone(),
                to: t.available.clone(),
                how: nyx_update::how(t).into(),
            })
            .collect(),
        notes: Vec::new(),
    };
    if settings.notify {
        nyx_update::run::notify_available(&st).await?;
    }
    Ok(())
}

/// The time of day here, as `date` has it (the system's time zone).
fn local_time() -> (u8, u8) {
    let out = std::process::Command::new("date").arg("+%H %M").output().ok();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mut it = text.split_whitespace().filter_map(|w| w.parse::<u8>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// The automatic install, once, at `h:m` (a transient user timer; one at a time).
fn schedule(h: u8, m: u8) {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "nyx-system".into());
    let _ = std::process::Command::new("systemd-run")
        .args(["--user", "--unit=nyx-update-install", "--collect", "--quiet"])
        .arg(format!("--on-calendar=*-*-* {h:02}:{m:02}:00"))
        .args([exe.as_str(), "update", "auto", "--now"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The last `limit` entries of the system's history and the user's, newest first.
async fn entries(limit: usize) -> Vec<HistoryEntry> {
    let system: Vec<HistoryEntry> = match crate::proxy().await {
        Ok(p) => p.history(limit as u32).await.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    nyx_update::run::merged_history(system, limit)
}

fn limit(rest: &[String], default: usize, usage: &str) -> Result<usize> {
    match rest {
        [] => Ok(default),
        [n] => n.parse().map_err(|_| anyhow::anyhow!("N is a number")),
        _ => bail!("{usage}"),
    }
}

async fn report(args: &[&str]) -> Result<()> {
    let (f, rest) = flags(args)?;
    let r = nyx_update::report(&entries(limit(&rest, 200, "nyx-system update report [N]")?).await);
    if f.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    if r.entries == 0 {
        println!("Nothing has been updated yet.");
        return Ok(());
    }
    println!(
        "The last {} update{} (since {}):",
        r.entries,
        if r.entries == 1 { "" } else { "s" },
        nyx_update::date(r.since.unwrap_or(0))
    );
    let rate = r.failure_rate().map(|p| format!(" ({p}% failed)")).unwrap_or_default();
    println!(
        "  System      {} updated, {} failed{rate}, {} already current",
        r.system_updated, r.system_failed, r.system_current
    );
    println!("  Going back  {} time{}, {} failed", r.rollbacks, if r.rollbacks == 1 { "" } else { "s" }, r.rollbacks_failed);
    println!("  Your own    {} updated, {} failed (Nix profile, Flatpaks, mise, uv, firmware)", r.user_updated, r.user_failed);
    println!("  Restarts    {} needed", r.restarts);
    let order = ["check", "pins", "download", "build", "switch", "total"];
    let typical: Vec<String> =
        order.iter().filter_map(|s| r.median.get(*s).map(|v| format!("{s} {}", nyx_update::duration(*v)))).collect();
    if !typical.is_empty() {
        println!("  Typical     {}", typical.join(" · "));
    }
    if let Some((request, time, secs)) = &r.longest {
        println!("  Longest     {} ({request}, {})", nyx_update::duration(*secs), nyx_update::date(*time));
    }
    Ok(())
}

async fn history(args: &[&str]) -> Result<()> {
    let (f, rest) = flags(args)?;
    let list = entries(limit(&rest, 20, "nyx-system update history [N]")?).await;
    if f.json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("Nothing has been updated yet.");
    }
    for e in &list {
        let generations = match (e.generation_before, e.generation_after) {
            (Some(a), Some(b)) if a != b => format!("  generation {a} → {b}"),
            _ => String::new(),
        };
        let took = e.seconds.get("total").map(|s| format!("  ({})", nyx_update::duration(*s))).unwrap_or_default();
        println!("{}  {:<8} {:<11} {}{generations}{took}", nyx_update::date(e.time), e.scope, e.result, e.request);
        for c in e.changes.iter().take(5) {
            println!("    {}: {} → {}", c.name, c.from, c.to);
        }
        if e.changes.len() > 5 {
            println!("    and {} more", e.changes.len() - 5);
        }
        if let Some(err) = &e.error {
            println!("    {err}");
        }
        if e.reboot {
            println!("    needed a restart");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_names() {
        let (f, names) = flags(&["claude", "--dry-run", "-y", "--all"]).unwrap();
        assert!(f.dry_run && f.yes && !f.json);
        assert_eq!(names, vec!["claude"]);
        assert!(flags(&["--frobnicate"]).is_err());
        assert!(wants_firmware(&[]) && wants_firmware(&["firmware".into()]) && !wants_firmware(&["claude".into()]));
    }
}

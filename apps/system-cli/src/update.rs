//! `nyx-system update` (and `nyx update`, which hands off to it): the plan, asked about
//! once, then carried out with its progress; `status` for what's available and `history`
//! for what was done. `--json` gives Settings and the Store the same plan and outcome
//! (`--yes --json`: no questions, the outcome on stdout, progress on stderr).

use anyhow::{Result, bail};
use nyx_system_proto::HistoryEntry;
use nyx_update::policy::{self, Network};
use nyx_update::run::Outcome;
use nyx_update::{Category, Item, Pick, Plan, Snapshot, Step, Target};
use std::io::IsTerminal;

pub const USAGE: &str = "  nyx-system update [TARGET...] [--dry-run] [--yes] [--json]
                                          Update everything, or: system (NixOS, Nyx apps),
                                          apps (Flatpaks, your Nix profile), tools
                                          (coding agents, your mise and uv tools),
                                          firmware, or one thing by
                                          name (claude, codex, firefox...). Plans
                                          first and asks; --dry-run only plans
  nyx-system update status [--notify] [--json]
                                          What has updates (--notify: as a notification,
                                          as the daily check does)
  nyx-system update history [N] [--json]  What was updated, newest first";

#[derive(Default)]
struct Flags {
    dry_run: bool,
    yes: bool,
    json: bool,
    notify: bool,
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
            // `nix profile upgrade --all`, as `nyx update` used to be.
            "--all" => {}
            a if a.starts_with('-') => bail!("unknown option {a}; see nyx-system --help"),
            a => names.push(a.to_string()),
        }
    }
    Ok((f, names))
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub async fn run(args: &[&str]) -> Result<()> {
    match args {
        [.., "--help" | "-h"] | ["help"] => {
            println!("{}", USAGE.replace("  nyx-system update", "  nyx update"));
            Ok(())
        }
        ["status", rest @ ..] => status(rest).await,
        ["history", rest @ ..] => history(rest).await,
        _ => update(args).await,
    }
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
        Network::Offline => bail!("there's no network connection, so nothing can be checked or downloaded"),
        Network::Metered if !f.json => eprintln!("This is a metered connection; updates can download a lot."),
        _ => {}
    }
    let settings = policy::Settings::load();
    if !f.json {
        eprintln!("Checking what's new…");
    }
    let snapshot = nyx_update::probe::probe(nyx_update::probe::Options {
        check: true,
        firmware: (settings.firmware && wants_firmware(&names)) || names.iter().any(|n| n == "firmware"),
    });
    let targets = nyx_update::targets(&snapshot);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let picks = nyx_update::resolve_installed(&snapshot, &targets, &refs)?;
    let mut plan = nyx_update::plan(&snapshot, &targets, &picks, &request);

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
            bail!("nothing was changed");
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
        std::process::exit(1);
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

async fn status(args: &[&str]) -> Result<()> {
    let (f, names) = flags(args)?;
    if !names.is_empty() {
        bail!("nyx-system update status takes no names");
    }
    let settings = policy::Settings::load();
    if f.notify && !settings.check {
        return Ok(());
    }
    if let Some(why) = policy::deferred(&settings, policy::network().await).filter(|_| f.notify) {
        println!("Not checking now: {why}.");
        return Ok(());
    }
    let snapshot = nyx_update::probe::probe(nyx_update::probe::Options { check: true, firmware: settings.firmware });
    let targets = nyx_update::targets(&snapshot);
    let st = nyx_update::status(&snapshot, &targets);
    if f.json {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            status: &'a nyx_update::Status,
            targets: &'a [Target],
            firmware: &'a nyx_update::Firmware,
        }
        println!("{}", serde_json::to_string_pretty(&Out { status: &st, targets: &targets, firmware: &snapshot.firmware })?);
    } else {
        if st.available.is_empty() {
            println!("Everything is up to date.");
        } else {
            println!("Updates available:");
            print_items(&st.available);
        }
        for n in &st.notes {
            println!("• {n}");
        }
    }
    if f.notify && settings.notify {
        nyx_update::run::notify_available(&st).await?;
    }
    Ok(())
}

async fn history(args: &[&str]) -> Result<()> {
    let (f, rest) = flags(args)?;
    let limit: usize = match rest.as_slice() {
        [] => 20,
        [n] => n.parse().map_err(|_| anyhow::anyhow!("N is a number"))?,
        _ => bail!("nyx-system update history [N]"),
    };
    let system: Vec<HistoryEntry> = match crate::proxy().await {
        Ok(p) => p.history(limit as u32).await.ok().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let list = nyx_update::run::merged_history(system, limit);
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
        println!("{}  {:<8} {:<7} {}{generations}", nyx_update::date(e.time), e.scope, e.result, e.request);
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

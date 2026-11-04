//! nyx-system: the System Manager and the configuration layers from a terminal.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use nyx_catalog::{Catalog, Installed, Tier};
use nyx_config::{Layer, Layers};
use nyx_system_proto::{Change, GROUPS, Generation, SERVICES, SystemProxy, SystemSettings};

const USAGE: &str = "\
nyx-system: NyxOS's system settings, packages, services and generations

  nyx-system settings                     The system layer's settings
  nyx-system set NAME VALUE               hostname, time-zone, locale, keyboard,
                                          theme, boot-theme, login-theme
  nyx-system unset NAME                   Back to the distribution's value
  nyx-system install [--accept-unfree] PACKAGE...
                                          nixpkgs packages for everyone (accepting
                                          the licenses of unfree ones)
  nyx-system remove PACKAGE...
  nyx-system services                     The services NyxOS manages here
  nyx-system service NAME on|off|default
  nyx-system app NAME on|off|default      A Nyx app (mail, media-player, ...)
  nyx-system groups                       The groups NyxOS manages here
  nyx-system group add|remove GROUP USER
  nyx-system update                       Update NyxOS and its apps, then switch
  nyx-system generations
  nyx-system rollback                     Back to the previous generation

  nyx-system catalog [SECTION]            The software catalog (* installed)
  nyx-system catalog presets
  nyx-system catalog install [--preset P]... [ID]... [--yes]
  nyx-system catalog remove ID...

  nyx-system default set DOMAIN KEY VALUE Every user's default (VALUE is TOML)
  nyx-system default reset DOMAIN KEY
  nyx-system config show DOMAIN           The merged settings, with layers
  nyx-system config get DOMAIN KEY [--below-user]
  nyx-system config set DOMAIN KEY VALUE  Your own setting (VALUE is TOML)
  nyx-system config reset DOMAIN KEY

Changes to the system are built first and only switched to when the build works.
";

fn change(name: &str, value: Option<String>) -> Result<Change> {
    Ok(match name {
        "hostname" => Change::Hostname { value },
        "time-zone" | "timezone" => Change::TimeZone { value },
        "locale" => Change::Locale { value },
        "keyboard" | "keyboard-layout" => Change::KeyboardLayout { value },
        "theme" => Change::SystemTheme { value },
        "boot-theme" => Change::BootTheme { value },
        "login-theme" => Change::LoginTheme { value },
        _ => bail!("unknown setting {name}; see nyx-system --help"),
    })
}

/// A TOML value as typed (`true`, `12`, `"x"`, `[1, 2]`), or else the text itself.
fn value(text: &str) -> toml::Value {
    format!("v = {text}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(text.into()))
}

async fn installed(p: &SystemProxy<'static>) -> Result<Installed> {
    let s: SystemSettings = serde_json::from_str(&p.settings().await?)?;
    Ok(Installed::probe(s))
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

async fn catalog(args: &[&str]) -> Result<()> {
    let c = Catalog::load()?;
    match args {
        [] | [_] if args.first().is_none_or(|s| c.section(s).is_some()) => {
            let have = installed(&proxy().await?).await?;
            for s in c.sections.iter().filter(|s| args.is_empty() || args[0] == s.id) {
                println!("{} ({})", s.name, s.id);
                for i in c.items_in(&s.id, &[Tier::Base, Tier::Welcome, Tier::Store]) {
                    let mark = if have.has(i) { "*" } else { " " };
                    let tier = match i.tier {
                        Tier::Base => "base",
                        Tier::Welcome => "",
                        Tier::Store => "store",
                    };
                    println!("  {mark} {:<16} {:<5} {}: {}", i.id, tier, i.name, i.summary);
                }
            }
        }
        ["presets"] => {
            for p in &c.presets {
                println!("{:<14} {}: {}\n               {}", p.id, p.name, p.summary, p.items.join(", "));
            }
        }
        ["install", rest @ ..] => {
            let yes = rest.contains(&"--yes");
            let mut ids = std::collections::BTreeSet::new();
            let mut it = rest.iter().filter(|a| **a != "--yes");
            while let Some(a) = it.next() {
                if *a == "--preset" {
                    ids.extend(c.presets(&[it.next().context("--preset needs a name")?])?);
                } else {
                    ids.insert(a.to_string());
                }
            }
            if ids.is_empty() {
                bail!("name items or presets to install");
            }
            let p = proxy().await?;
            let user = std::env::var("USER").context("USER isn't set")?;
            let plan = c.plan(&ids, &user, &installed(&p).await?)?;
            if plan.is_empty() {
                println!("Everything's already installed.");
                return Ok(());
            }
            for ch in &plan.changes {
                println!("  {}", serde_json::to_string(ch)?);
            }
            for f in &plan.flatpaks {
                println!("  flathub {f} (for you)");
            }
            for consent in &plan.consent {
                println!("! {}", consent.describe());
            }
            if !yes && !confirm(if plan.rebuilds() { "Install? The system is rebuilt once." } else { "Install?" })? {
                bail!("nothing was changed");
            }
            if plan.rebuilds() {
                let json = serde_json::to_string(&plan.changes)?;
                println!("{}", with_progress(&p, p.apply(&json)).await?);
            }
            nyx_catalog::flatpak(&plan.flatpaks, true, |l| eprintln!("{l}"))?;
        }
        ["remove", ids @ ..] if !ids.is_empty() => {
            let p = proxy().await?;
            let ids = ids.iter().map(|s| s.to_string()).collect();
            let plan = c.removal(&ids, &installed(&p).await?)?;
            if plan.is_empty() {
                println!("None of those are installed through NyxOS.");
                return Ok(());
            }
            if plan.rebuilds() {
                let json = serde_json::to_string(&plan.changes)?;
                println!("{}", with_progress(&p, p.apply(&json)).await?);
            }
            nyx_catalog::flatpak(&plan.flatpaks, false, |l| eprintln!("{l}"))?;
        }
        _ => bail!("see nyx-system --help"),
    }
    Ok(())
}

fn on_off(state: &str) -> Result<Option<bool>> {
    Ok(match state {
        "on" => Some(true),
        "off" => Some(false),
        "default" => None,
        _ => bail!("say on, off or default"),
    })
}

async fn proxy() -> Result<SystemProxy<'static>> {
    let conn = zbus::Connection::system().await.context("can't reach the system bus")?;
    Ok(SystemProxy::new(&conn).await?)
}

/// Run a long call while printing the manager's progress.
async fn with_progress<F, T>(p: &SystemProxy<'static>, call: F) -> Result<T>
where
    F: std::future::Future<Output = zbus::Result<T>>,
{
    let mut lines = p.receive_progress().await?;
    let printer = tokio::spawn(async move {
        while let Some(signal) = lines.next().await {
            if let Ok(args) = signal.args() {
                eprintln!("{}", args.line());
            }
        }
    });
    let result = call.await;
    printer.abort();
    Ok(result?)
}

async fn apply(changes: Vec<Change>) -> Result<()> {
    for c in &changes {
        c.validate()?;
    }
    let p = proxy().await?;
    let json = serde_json::to_string(&changes)?;
    println!("{}", with_progress(&p, p.apply(&json)).await?);
    Ok(())
}

fn print_layers(table: &toml::Table, domain: &str, layers: &Layers, prefix: &str) -> Result<()> {
    for (k, v) in table {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            toml::Value::Table(t) => print_layers(t, domain, layers, &key)?,
            v => {
                let layer = layers.get(domain, &key)?.map(|(_, l)| l.name()).unwrap_or("");
                println!("{key} = {v}  ({layer})");
            }
        }
    }
    Ok(())
}

async fn run(args: &[&str]) -> Result<()> {
    let layers = Layers::standard();
    match args {
        [] | ["help" | "--help" | "-h"] => print!("{USAGE}"),
        ["settings"] => {
            let s: SystemSettings = serde_json::from_str(&proxy().await?.settings().await?)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        ["set", name, v] => apply(vec![change(name, Some(v.to_string()))?]).await?,
        ["unset", name] => apply(vec![change(name, None)?]).await?,
        ["install", "--accept-unfree", packages @ ..] if !packages.is_empty() => {
            let packages: Vec<String> = packages.iter().map(|s| s.to_string()).collect();
            apply(vec![Change::AddPackages { unfree: packages.clone(), packages }]).await?
        }
        ["install", packages @ ..] if !packages.is_empty() => {
            apply(vec![Change::AddPackages { packages: packages.iter().map(|s| s.to_string()).collect(), unfree: Vec::new() }])
                .await?
        }
        ["remove", packages @ ..] if !packages.is_empty() => {
            apply(vec![Change::RemovePackages { packages: packages.iter().map(|s| s.to_string()).collect() }]).await?
        }
        ["services"] => {
            let s: SystemSettings = serde_json::from_str(&proxy().await?.settings().await?)?;
            for (name, what) in SERVICES {
                let state = match s.services.get(*name) {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "default",
                };
                println!("{name:<10} {state:<8} {what}");
            }
        }
        ["service", name, state] => apply(vec![Change::Service { name: name.to_string(), enabled: on_off(state)? }]).await?,
        ["app", name, state] => apply(vec![Change::NyxApp { name: name.to_string(), enabled: on_off(state)? }]).await?,
        ["groups"] => {
            let s: SystemSettings = serde_json::from_str(&proxy().await?.settings().await?)?;
            for (name, what) in GROUPS {
                let users = s.groups.get(*name).map(|u| u.iter().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
                println!("{name:<12} {what}{}", if users.is_empty() { String::new() } else { format!(": {users}") });
            }
        }
        ["group", how @ ("add" | "remove"), group, user] => {
            apply(vec![Change::GroupMember { group: group.to_string(), user: user.to_string(), member: *how == "add" }]).await?
        }
        ["update"] => {
            let p = proxy().await?;
            println!("{}", with_progress(&p, p.update()).await?);
        }
        ["rollback"] => {
            let p = proxy().await?;
            println!("{}", with_progress(&p, p.rollback()).await?);
        }
        ["generations"] => {
            let list: Vec<Generation> = serde_json::from_str(&proxy().await?.generations().await?)?;
            for g in list {
                println!("{}{:>5}  {}  {}", if g.current { "*" } else { " " }, g.number, g.created, g.version);
            }
        }
        ["catalog", rest @ ..] => catalog(rest).await?,
        ["default", "set", domain, key, v] => {
            proxy().await?.set_default(domain, key, &serde_json::to_string(&value(v))?).await?;
        }
        ["default", "reset", domain, key] => proxy().await?.reset_default(domain, key).await?,
        ["config", "show", domain] => print_layers(&layers.merged(domain)?, domain, &layers, "")?,
        ["config", "get", domain, key, rest @ ..] => {
            // --below-user: the value a new user starts with, from defaults and the system.
            let probe = match rest {
                ["--below-user"] => Layers { user: std::path::PathBuf::from("/nonexistent"), ..layers.clone() },
                [] => layers.clone(),
                _ => bail!("unknown option {}", rest.join(" ")),
            };
            match probe.get(domain, key)? {
                Some((toml::Value::String(s), _)) => println!("{s}"),
                Some((v, _)) => println!("{v}"),
                None => std::process::exit(1),
            }
        }
        ["config", "set", domain, key, v] => layers.set(Layer::User, domain, key, value(v))?,
        ["config", "reset", domain, key] => layers.reset(Layer::User, domain, key)?,
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = run(&args).await {
        eprintln!("nyx-system: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn values_are_toml_or_text() {
        assert_eq!(super::value("true"), toml::Value::Boolean(true));
        assert_eq!(super::value("12"), toml::Value::Integer(12));
        assert_eq!(super::value("nord"), toml::Value::String("nord".into()));
        assert_eq!(super::value("\"a b\""), toml::Value::String("a b".into()));
    }
}

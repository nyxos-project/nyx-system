//! nyx-system: the System Manager and the configuration layers from a terminal.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use nyx_config::{Layer, Layers};
use nyx_system_proto::{Change, Generation, SERVICES, SystemProxy, SystemSettings};

const USAGE: &str = "\
nyx-system: NyxOS's system settings, packages, services and generations

  nyx-system settings                     The system layer's settings
  nyx-system set NAME VALUE               hostname, time-zone, locale, keyboard,
                                          theme, boot-theme, login-theme
  nyx-system unset NAME                   Back to the distribution's value
  nyx-system install PACKAGE...           nixpkgs packages for everyone
  nyx-system remove PACKAGE...
  nyx-system services                     The services NyxOS manages here
  nyx-system service NAME on|off|default
  nyx-system update                       Update NyxOS and its apps, then switch
  nyx-system generations
  nyx-system rollback                     Back to the previous generation

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
        ["install", packages @ ..] if !packages.is_empty() => {
            apply(vec![Change::AddPackages { packages: packages.iter().map(|s| s.to_string()).collect() }]).await?
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
        ["service", name, state] => {
            let enabled = match *state {
                "on" => Some(true),
                "off" => Some(false),
                "default" => None,
                _ => bail!("say on, off or default"),
            };
            apply(vec![Change::Service { name: name.to_string(), enabled }]).await?
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

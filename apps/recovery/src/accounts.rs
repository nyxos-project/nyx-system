//! Accounts: the people who can sign in, a new password, and unlocking an account that
//! was locked or ran out of tries.

use crate::paths::Paths;
use crate::run::{Log, run};
use anyhow::{Result, ensure};

#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub login: String,
    pub name: String,
    pub uid: u32,
    pub home: String,
    /// The password is switched off (`!` in /etc/shadow).
    pub locked: bool,
}

/// People from /etc/passwd: ordinary uids with a login shell.
pub fn people(passwd: &str, shadow: &str) -> Vec<Person> {
    let locked: Vec<&str> =
        shadow.lines().filter_map(|l| l.split_once(':')).filter(|(_, rest)| rest.starts_with('!')).map(|(u, _)| u).collect();
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let shell = f[6];
            let person = (1000..60000).contains(&uid) && !shell.ends_with("nologin") && !shell.ends_with("/false");
            person.then(|| Person {
                login: f[0].into(),
                name: f[4].split(',').next().filter(|n| !n.is_empty()).unwrap_or(f[0]).into(),
                uid,
                home: f[5].into(),
                locked: locked.contains(&f[0]),
            })
        })
        .collect()
}

pub fn all(p: &Paths) -> Vec<Person> {
    people(&std::fs::read_to_string(&p.passwd).unwrap_or_default(), &std::fs::read_to_string(&p.shadow).unwrap_or_default())
}

fn valid_login(login: &str) -> bool {
    !login.is_empty() && login.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !login.starts_with('-')
}

pub fn set_password(login: &str, password: &str, log: &Log) -> Result<String> {
    ensure!(valid_login(login), "that isn't a login name");
    ensure!(!password.is_empty() && !password.contains(['\n', ':']), "choose a password without line breaks or colons");
    run(log, &["chpasswd"], Some(&format!("{login}:{password}\n")))?;
    Ok(format!("{login} has a new password."))
}

/// Turn the password back on and forget the failed tries.
pub fn unlock(login: &str, log: &Log) -> Result<String> {
    ensure!(valid_login(login), "that isn't a login name");
    let _ = run(log, &["usermod", "--unlock", login], None);
    run(log, &["faillock", "--user", login, "--reset"], None)?;
    Ok(format!("{login} can sign in again."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_are_ordinary_accounts_with_a_shell() {
        let passwd = "root:x:0:0:System administrator:/root:/bin/bash\n\
                      nixbld1:x:30001:30000:Nix build user 1:/var/empty:/run/current-system/sw/bin/nologin\n\
                      ken:x:1000:100:Ken Smith:/home/ken:/run/current-system/sw/bin/zsh\n\
                      amy:x:1001:100::/home/amy:/bin/bash\n\
                      greeter:x:996:996::/var/lib/nyx-greeter:/bin/false\n";
        let shadow = "ken:$y$j9T$abc:20000::::::\namy:!$y$xyz:20000::::::\n";
        let p = people(passwd, shadow);
        assert_eq!(p.iter().map(|x| x.login.as_str()).collect::<Vec<_>>(), vec!["ken", "amy"]);
        assert_eq!(p[0].name, "Ken Smith");
        assert_eq!(p[1].name, "amy");
        assert!(!p[0].locked && p[1].locked);
    }

    #[test]
    fn bad_input_is_refused_before_anything_runs() {
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(set_password("ken", "a:b", &tx).is_err());
        assert!(set_password("ken", "", &tx).is_err());
        assert!(set_password("-rf", "x", &tx).is_err());
        assert!(unlock("ken;reboot", &tx).is_err());
    }
}

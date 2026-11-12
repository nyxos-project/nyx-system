//! Who may use Recovery: an administrator (a member of `wheel`, or root) who signs in
//! with their password, as `sulogin` asks before a rescue shell. When no administrator
//! has a password there's nothing to ask for, and Recovery opens.

use crate::paths::Paths;
use std::ffi::{CStr, CString};
use std::io::{BufRead, Write};

#[link(name = "crypt")]
unsafe extern "C" {
    fn crypt(phrase: *const libc::c_char, setting: *const libc::c_char) -> *mut libc::c_char;
}

/// Administrators with a password, and its hash from /etc/shadow.
pub fn admins(group: &str, shadow: &str) -> Vec<(String, String)> {
    let wheel: Vec<&str> = group
        .lines()
        .find_map(|l| l.strip_prefix("wheel:"))
        .and_then(|rest| rest.splitn(3, ':').nth(2))
        .map(|members| members.split(',').filter(|m| !m.is_empty()).collect())
        .unwrap_or_default();
    shadow
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, ':');
            let (login, hash) = (f.next()?, f.next()?);
            let usable = hash.starts_with('$') && !hash.contains(['!', '*']);
            (usable && (login == "root" || wheel.contains(&login))).then(|| (login.to_string(), hash.to_string()))
        })
        .collect()
}

/// Whether `password` matches `hash` (any scheme libxcrypt knows: yescrypt, SHA-512, …).
pub fn matches(password: &str, hash: &str) -> bool {
    let (Ok(phrase), Ok(setting)) = (CString::new(password), CString::new(hash)) else {
        return false;
    };
    // SAFETY: both are NUL-terminated; crypt returns a static buffer (or NULL), read at once.
    let out = unsafe { crypt(phrase.as_ptr(), setting.as_ptr()) };
    if out.is_null() {
        return false;
    }
    let out = unsafe { CStr::from_ptr(out) }.to_bytes();
    out.len() == hash.len() && out.iter().zip(hash.as_bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

/// Ask on the console until an administrator signs in. Returns at once when none has a
/// password.
pub fn require(p: &Paths) {
    let group = std::fs::read_to_string(&p.group).unwrap_or_default();
    let shadow = std::fs::read_to_string(&p.shadow).unwrap_or_default();
    let admins = admins(&group, &shadow);
    if admins.is_empty() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = writeln!(out, "NyxOS Recovery\n");
    let _ = writeln!(out, "Sign in as an administrator to repair this computer.");
    let _ = writeln!(out, "Forgot the password? Start from the NyxOS installer USB to reset it.\n");
    let only = (admins.len() == 1).then(|| admins[0].0.clone());
    let mut tries = 0u32;
    loop {
        let login = match &only {
            Some(login) => {
                let _ = writeln!(out, "Administrator: {login}");
                login.clone()
            }
            None => {
                let _ = write!(out, "Administrator: ");
                let _ = out.flush();
                read_line(false)
            }
        };
        let _ = write!(out, "Password: ");
        let _ = out.flush();
        let password = zeroize_on_drop(read_line(true));
        let _ = writeln!(out);
        if admins.iter().any(|(l, hash)| *l == login && matches(&password.0, hash)) {
            return;
        }
        tries += 1;
        let _ = writeln!(out, "That isn't an administrator's password.\n");
        std::thread::sleep(std::time::Duration::from_secs(if tries >= 3 { 5 } else { 2 }));
    }
}

struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: overwriting the string's own bytes with zeros keeps it valid UTF-8.
        unsafe { self.0.as_bytes_mut().fill(0) };
    }
}

fn zeroize_on_drop(s: String) -> Secret {
    Secret(s)
}

/// One line from the console, without echo for a password.
fn read_line(secret: bool) -> String {
    // SAFETY: tcgetattr/tcsetattr on stdin with a termios we own.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (secret && libc::tcgetattr(0, &mut t) == 0).then_some(t)
    };
    if let Some(t) = saved {
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if let Some(t) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    line.trim_end_matches(['\r', '\n']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn administrators_are_wheel_members_and_root_with_a_password() {
        let group = "root:x:0:\nwheel:x:1:ken,amy\nusers:x:100:bob\n";
        let shadow = "root:!:1::::::\nken:$y$j9T$salt$hash:20000::::::\namy:!$y$x$y:20000::::::\nbob:$6$s$h:20000::::::\n";
        assert_eq!(admins(group, shadow), vec![("ken".to_string(), "$y$j9T$salt$hash".to_string())]);
        let shadow = "root:$6$r$h:1::::::\n";
        assert_eq!(admins(group, shadow).len(), 1);
        assert!(admins("wheel:x:1:\n", "ken:$y$a$b:1::::::\n").is_empty());
    }

    #[test]
    fn passwords_match_their_hash() {
        // "secret" in SHA-512 crypt and in yescrypt, NixOS's default.
        let sha = "$6$saltsalt$TVLlQcbpFVof5W3Yz4DTP6gRstiNuHwwTt6GLc1E5n0U0aDehy0S5knV8wiOQSpT0Y77vwPZN.Pq.H91p5hVO1";
        let yes = "$y$j9T$QRgoBDpw4x1rgG0YzrkHL/$iaeV6g3ZHIs3jYzh6ZgmBuz4p7P0PJjNXoHmguIi43.";
        for hash in [sha, yes] {
            assert!(matches("secret", hash), "{hash}");
            assert!(!matches("Secret", hash), "{hash}");
        }
        assert!(!matches("secret", "!"));
    }
}

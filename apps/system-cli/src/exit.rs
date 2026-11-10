//! Exit codes, the same for every command, for scripts and automation:
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | done, or nothing to do |
//! | 1 | failed |
//! | 2 | not a command nyx-system knows (usage) |
//! | 3 | partly done: some of it worked, what failed was left as it was |
//! | 4 | a name that isn't installed or known, or that fits several things |
//! | 5 | no network connection |
//! | 6 | another update or system change is running |
//! | 7 | declined: the person said no, or couldn't be asked |

use std::io::{IsTerminal, Write};

pub const FAILED: i32 = 1;
pub const USAGE: i32 = 2;
pub const PARTIAL: i32 = 3;
pub const NAME: i32 = 4;
pub const OFFLINE: i32 = 5;
pub const BUSY: i32 = 6;
pub const DECLINED: i32 = 7;

/// An error with its exit code.
#[derive(Debug)]
pub struct Exit {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Exit {}

pub fn fail(code: i32, message: impl Into<String>) -> anyhow::Error {
    Exit { code, message: message.into() }.into()
}

/// The exit code for `e`.
pub fn code(e: &anyhow::Error) -> i32 {
    if let Some(x) = e.chain().find_map(|c| c.downcast_ref::<Exit>()) {
        return x.code;
    }
    let text = format!("{e:#}");
    if text.contains("another update is already running") || text.contains("another system change is running") {
        BUSY
    } else {
        FAILED
    }
}

/// Which of `options` (a name and what it is) `name` means: asked on a terminal when
/// `ask`, otherwise an error listing them, so a script names the one it means.
pub fn choose(name: &str, options: &[(String, String)], ask: bool) -> anyhow::Result<usize> {
    let list: Vec<String> = options.iter().map(|(id, what)| format!("  {id:<24} {what}")).collect();
    if !ask || !std::io::stdin().is_terminal() {
        return Err(fail(NAME, format!("{name} could be any of these; name the one you mean:\n{}", list.join("\n"))));
    }
    println!("{name} could be any of these:");
    for (n, (id, what)) in options.iter().enumerate() {
        println!("  {}) {id:<24} {what}", n + 1);
    }
    print!("Which one? [1-{}, Enter for none] ", options.len());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    match answer.trim().parse::<usize>() {
        Ok(n) if (1..=options.len()).contains(&n) => Ok(n - 1),
        _ => Err(fail(DECLINED, "nothing was chosen, so nothing was changed")),
    }
}

/// Yes or no on a terminal; no when nobody can be asked.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(code(&fail(NAME, "x")), NAME);
        assert_eq!(code(&anyhow::anyhow!("another update is already running")), BUSY);
        assert_eq!(code(&fail(OFFLINE, "x").context("checking")), OFFLINE);
        assert_eq!(code(&anyhow::anyhow!("boom")), FAILED);
        let opts = vec![("cursor".to_string(), "Cursor".to_string()), ("cursor-agent".to_string(), "Cursor Agent".to_string())];
        assert_eq!(code(&choose("cursor", &opts, false).unwrap_err()), NAME);
    }
}

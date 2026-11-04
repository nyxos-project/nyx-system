//! Running a command for a job: each line it prints goes to the screen, and a failure
//! says why (its first `error:` line, else its last).

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

/// What a job prints as it goes.
pub type Log = Sender<String>;

pub fn run(log: &Log, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let _ = log.send(format!("$ {}", args.join(" ")));
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't start {}", args[0]))?;
    if let Some(text) = stdin {
        let mut s = child.stdin.take().context("no stdin")?;
        s.write_all(text.as_bytes())?;
    }
    let err = child.stderr.take().context("no stderr")?;
    let log2 = log.clone();
    let errors = std::thread::spawn(move || {
        let mut first_error = String::new();
        let mut last = String::new();
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let t = line.trim();
            if first_error.is_empty() && t.starts_with("error:") {
                first_error = t.trim_start_matches("error:").trim().to_string();
            }
            if !t.is_empty() {
                last = t.to_string();
            }
            let _ = log2.send(line);
        }
        if first_error.is_empty() { last } else { first_error }
    });
    let mut out = String::new();
    for line in BufReader::new(child.stdout.take().context("no stdout")?).lines().map_while(Result::ok) {
        let _ = log.send(line.clone());
        out.push_str(&line);
        out.push('\n');
    }
    let status = child.wait()?;
    let why = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("{} failed: {}", args[0], if why.is_empty() { status.to_string() } else { why });
    }
    Ok(out)
}

/// Run without showing the output (for reading state).
pub fn quiet(args: &[&str]) -> Result<String> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("can't start {}", args[0]))?;
    if !out.status.success() {
        bail!("{} failed: {}", args[0], String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

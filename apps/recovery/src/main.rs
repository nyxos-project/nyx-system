//! NyxOS Recovery: a text console for when the desktop won't start or something needs
//! repairing. It works offline and without AI. Generations (boot an earlier one),
//! configuration repair (undo the last system change, rebuild, reset the settings layer),
//! health (disk space, failures, the last boot's errors, making room), accounts (a new
//! password, unlocking) and restoring files from a Nyx Backup repository.
//!
//! The "NyxOS Recovery" boot entry runs it as root on the first console. Nothing switches
//! the running system: what it changes takes effect at the next start.

mod accounts;
mod health;
mod paths;
mod restore;
mod run;
mod system;
mod ui;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("nyx-recovery {}", env!("CARGO_PKG_VERSION"));
            return std::process::ExitCode::SUCCESS;
        }
        Some("-h" | "--help") => {
            println!(
                "Usage: nyx-recovery\n\n\
                 NyxOS Recovery, for the \"NyxOS Recovery\" boot entry (or as root from a console).\n\
                 Generations, configuration repair, health, accounts and restoring from a backup,\n\
                 all without the network. Changes take effect at the next start."
            );
            return std::process::ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("nyx-recovery: unknown option {other}");
            return std::process::ExitCode::from(2);
        }
        None => {}
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("nyx-recovery: run it as root (the NyxOS Recovery boot entry does)");
        return std::process::ExitCode::from(1);
    }
    match ui::run(paths::Paths::system()) {
        Ok(ui::Exit::Quit) => std::process::ExitCode::SUCCESS,
        Ok(ui::Exit::Reboot) => {
            let _ = std::process::Command::new("systemctl").arg("reboot").status();
            std::process::ExitCode::SUCCESS
        }
        Ok(ui::Exit::PowerOff) => {
            let _ = std::process::Command::new("systemctl").arg("poweroff").status();
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("nyx-recovery: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

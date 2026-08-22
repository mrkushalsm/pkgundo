//! One-shot `pkgundo setup` / `pkgundo setup --remove`: the daily-user
//! equivalent of manually copying the systemd unit, patching its exec path,
//! enabling+starting the daemon, and separately running `install-hook`. A
//! fresh install today needs all of that done by hand (as every VM test
//! script in `scripts/vm-test/` still does); `setup` collapses it into one
//! idempotent command so `install.sh` has a single thing to call after it
//! places the binary.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::path::Path;
use std::process::Command;

use crate::commands::hook::handle_install_hook;
use crate::commands::is_root;

const DAEMON_UNIT_PATH: &str = "/etc/systemd/system/pkgundo-daemon.service";
const DAEMON_UNIT_TEMPLATE: &str = include_str!("../../systemd/pkgundo-daemon.service");

pub fn handle_setup(remove: bool) -> Result<()> {
    if !is_root() {
        bail!("pkgundo setup requires root privileges.");
    }

    if remove {
        run_systemctl(&["disable", "--now", "pkgundo-daemon"]);
        if Path::new(DAEMON_UNIT_PATH).exists() {
            std::fs::remove_file(DAEMON_UNIT_PATH)
                .with_context(|| format!("Failed to remove {}", DAEMON_UNIT_PATH))?;
            println!("{} Removed {}", "✓".green(), DAEMON_UNIT_PATH);
        } else {
            println!("{} No daemon unit installed at {}", "→".yellow(), DAEMON_UNIT_PATH);
        }
        run_systemctl(&["daemon-reload"]);
        handle_install_hook(true)?;
        println!("{} pkgundo setup removed: daemon stopped/disabled, hooks removed.", "✓".green());
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to resolve pkgundo's own executable path")?;
    let exe_str = exe.to_string_lossy();

    let contents = DAEMON_UNIT_TEMPLATE.replace("/usr/bin/pkgundo", &exe_str);
    std::fs::write(DAEMON_UNIT_PATH, contents).with_context(|| format!("Failed to write {}", DAEMON_UNIT_PATH))?;
    println!("{} Installed daemon unit at {} (ExecStart = {})", "✓".green(), DAEMON_UNIT_PATH, exe_str);

    run_systemctl(&["daemon-reload"]);
    if !run_systemctl(&["enable", "--now", "pkgundo-daemon"]) {
        bail!("Failed to enable/start pkgundo-daemon — check 'systemctl status pkgundo-daemon' for details.");
    }
    println!("{} pkgundo-daemon is enabled and running.", "✓".green());

    handle_install_hook(false)?;

    println!(
        "\n{} pkgundo is set up: the daemon is running and will start on every boot,\n  and package-manager hooks are installed. Run 'pkgundo track <app>' to start\n  watching an app's $HOME footprint.",
        "✓".green()
    );
    Ok(())
}

/// Runs `systemctl <args>`, returning whether it succeeded. Failures during
/// `--remove` (e.g. the unit was never enabled) are expected and non-fatal —
/// only the install path's `enable --now` treats a failure as an error.
fn run_systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").args(args).status().map(|s| s.success()).unwrap_or(false)
}

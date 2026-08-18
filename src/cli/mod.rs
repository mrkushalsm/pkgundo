use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "pkgundo",
    version = "0.1.0",
    author = "pkgundo contributors",
    about = "Universal Linux transaction monitor, mutation provenance engine, and intelligent rollback system",
    long_about = "pkgundo tracks all system mutations caused by package installations, scripts, and commands.\nIt enables intelligent, safe rollback of those changes while cooperating with native package managers."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run a command under pkgundo monitoring. Creates a transaction that tracks all mutations.
    /// Example: pkgundo run pacman -S steam
    Run {
        /// The command and its arguments to execute under monitoring
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,

        /// Rollback mode to use if rollback is triggered (conservative|clean|nuclear)
        #[arg(long, default_value = "conservative")]
        mode: String,
    },

    /// Roll back a previously recorded transaction, restoring the system to its pre-transaction state.
    /// Example: pkgundo rollback 42
    Rollback {
        /// Transaction ID to roll back
        txid: i64,

        /// Rollback mode (conservative|clean|nuclear)
        #[arg(long, default_value = "conservative")]
        mode: String,

        /// Dry run: show what would happen without making changes
        #[arg(long, default_value = "false")]
        dry_run: bool,
    },

    /// Inspect the details of a specific transaction: mutations, files, services, etc.
    /// Example: pkgundo inspect 42
    Inspect {
        /// Transaction ID to inspect
        txid: i64,
    },

    /// Show a timeline of all recorded transactions in chronological order.
    Timeline,

    /// Show current status of pkgundo: recent transactions, active monitors, etc.
    Status,

    /// Recover archived files from a previous transaction rollback.
    /// Example: pkgundo recover 42
    Recover {
        /// Transaction ID whose archives to recover
        txid: i64,
    },

    /// Simulate what would happen if you ran a command (dry-run, no actual changes).
    /// Example: pkgundo simulate pacman -S nginx
    Simulate {
        /// The command and arguments to simulate
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },

    /// Track an app so pkgundo daemon watches it across its whole life.
    /// Example: pkgundo track firefox
    Track {
        /// Package name or binary path/name to track
        app: String,
    },

    /// Stop tracking a previously tracked app.
    /// Example: pkgundo untrack firefox
    Untrack {
        /// Package name or binary path/name to stop tracking
        app: String,
    },

    /// List tracked apps.
    Tracked {
        /// Include untracked (historical) entries, not just currently-tracking apps
        #[arg(long, default_value = "false")]
        all: bool,
    },

    /// Scan for leftover files of an app under $HOME (installed or already removed).
    /// Example: pkgundo scan-leftovers firefox --dry-run
    ScanLeftovers {
        /// Package name or app identifier to scan for
        app: String,

        /// Only print candidates, don't archive/remove anything
        #[arg(long, default_value = "false")]
        dry_run: bool,
    },

    /// Run the pkgundo daemon (used by systemd; not intended for direct interactive use).
    #[command(hide = true)]
    Daemon,
}

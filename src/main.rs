use clap::{Parser, Subcommand};

/// Path of Exile price-check overlay for the COSMIC desktop.
#[derive(Parser)]
#[command(name = "poechk", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-lived daemon (owns config, cache, rate limiters, IPC service).
    Daemon,
    /// Render the overlay for a result JSON file (spawned by the daemon; also handy for testing).
    Overlay {
        /// Path to a JSON `PriceCheckResult` to display.
        #[arg(long, value_name = "PATH")]
        result: std::path::PathBuf,
    },
    /// Price-check the current clipboard item. Bind this to a COSMIC shortcut.
    Check,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Daemon => poechk::daemon::run(),
        Command::Overlay { result } => poechk::overlay::run_from_file(&result),
        Command::Check => poechk::check::run(),
    }
}

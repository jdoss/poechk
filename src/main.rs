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
    /// Render the overlay for a saved item JSON file (development aid).
    Overlay {
        /// Path to a JSON `ParsedItem` to display.
        #[arg(long, value_name = "PATH")]
        result: std::path::PathBuf,
    },
    /// Price-check the current clipboard item. Bind this to a COSMIC shortcut.
    Check {
        /// First copy the hovered item by injecting Ctrl+Alt+C into the game.
        #[arg(long)]
        copy: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Overlay { result } => poechk::overlay::run_from_file(&result),
        Command::Check { copy } => poechk::check::run(copy),
    }
}

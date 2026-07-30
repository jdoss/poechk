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

/// Send tracing to stderr and, when the log directory is writable, to
/// `poechk.log` beside the check log — a check run from a desktop shortcut has
/// no stderr to read, so the file is the only copy anyone sees.
fn init_tracing() {
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    let to_file = poechk::checklog::trace_file().map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .with_filter(filter())
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter()))
        .with(to_file)
        .init();
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    match Cli::parse().command {
        Command::Overlay { result } => poechk::overlay::run_from_file(&result),
        Command::Check { copy } => poechk::check::run(copy),
    }
}

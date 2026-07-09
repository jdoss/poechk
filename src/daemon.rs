//! The long-lived daemon: owns config, the price cache, rate limiters, and the
//! IPC service, and spawns the overlay on demand. Implemented in milestone M1.

/// Run the daemon.
pub fn run() -> anyhow::Result<()> {
    let cfg = crate::config::load()?;
    tracing::info!(game = ?cfg.game, league = %cfg.league, "poechk daemon (scaffold)");
    anyhow::bail!("daemon not implemented yet — see DESIGN.md (milestone M1)")
}

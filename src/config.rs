//! User configuration, stored as TOML under the XDG config directory.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::item::Game;

/// Persisted user settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which game to parse and price for.
    pub game: Game,
    /// Trade league, e.g. "Standard" or the current challenge league.
    pub league: String,
    /// Trade realm: "pc-ggg" or "pc-garena".
    pub realm: String,
    /// Optional POESESSID cookie to raise trade-API rate limits.
    pub poesessid: Option<String>,
    /// Extra seconds added to every rate-limit window as a safety margin.
    pub api_latency_seconds: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            game: Game::default(),
            league: "Standard".to_string(),
            realm: "pc-ggg".to_string(),
            poesessid: None,
            api_latency_seconds: 0.5,
        }
    }
}

/// Path to `config.toml` under the user's XDG config directory.
pub fn config_path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "jdoss", "poechk")
        .context("could not locate an XDG config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Load the config, writing defaults on first run.
pub fn load() -> anyhow::Result<Config> {
    let path = config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            save(&cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Persist the config, creating the config directory if needed.
pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

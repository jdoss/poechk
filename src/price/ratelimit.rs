//! A tiny file-based rate limiter shared across `check` processes.
//!
//! The trade API is strict, so before each request we wait until the endpoint's
//! window has elapsed, and after each response we push the next-allowed time
//! forward using GGG's advertised limits. State lives in one JSON file so
//! separate `check` invocations coordinate. (The daemon will subsume this.)

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Endpoint -> earliest unix time (seconds) a request is allowed.
    next_allowed: HashMap<String, f64>,
}

/// Coordinates trade-API request pacing via a shared state file.
pub struct RateLimiter {
    path: PathBuf,
}

impl RateLimiter {
    /// Open the limiter, storing state under the app cache directory.
    pub fn open() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("io.github", "jdoss", "poechk")
            .context("could not locate a cache directory")?;
        let dir = dirs.cache_dir();
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            path: dir.join("ratelimit.json"),
        })
    }

    /// Block until `endpoint`'s window allows a request.
    pub fn wait(&self, endpoint: &str) {
        if let Some(&next) = self.load().next_allowed.get(endpoint) {
            let now = now_secs();
            if next > now {
                std::thread::sleep(Duration::from_secs_f64(next - now));
            }
        }
    }

    /// Record that a request was made; the next is allowed after `interval`.
    pub fn record(&self, endpoint: &str, interval: Duration) {
        let mut state = self.load();
        state
            .next_allowed
            .insert(endpoint.to_string(), now_secs() + interval.as_secs_f64());
        if let Err(e) = self.save(&state) {
            tracing::warn!("could not persist rate-limit state: {e}");
        }
    }

    fn load(&self) -> State {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &State) -> anyhow::Result<()> {
        let text = serde_json::to_string(state)?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

//! A tiny file-based rate limiter shared across `check` processes.
//!
//! GGG advertises several limits per endpoint at once, as `max:window` pairs —
//! e.g. 5 requests per 10s *and* 600 per 6 hours. These are sliding windows, not
//! a required spacing: bursting five searches back to back is allowed, and only
//! the sixth within ten seconds has to wait. So the limiter keeps the times of
//! recent requests and delays only when a window is actually full. Pacing every
//! request at the slowest bucket's average rate instead would put 36s between
//! searches to satisfy a budget that is 43/600 used.
//!
//! State lives in one JSON file so separate `check` invocations coordinate.
//! (The daemon will subsume this.)

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// One advertised limit: at most `max` requests per `window` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub max: u32,
    pub window: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Endpoint -> the limits GGG last advertised for it.
    #[serde(default)]
    limits: HashMap<String, Vec<Bucket>>,
    /// Endpoint -> unix times (seconds) of recent requests, oldest first.
    #[serde(default)]
    history: HashMap<String, Vec<f64>>,
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

    /// Block until `endpoint` has room in every advertised window. `margin`
    /// pads each wait to absorb the round-trip GGG measures but we don't.
    pub fn wait(&self, endpoint: &str, margin: f64) {
        let state = self.load();
        let (Some(limits), Some(history)) =
            (state.limits.get(endpoint), state.history.get(endpoint))
        else {
            return;
        };
        let delay = delay_for(limits, history, now_secs(), margin);
        if delay > 0.0 {
            tracing::debug!(endpoint, delay, "rate limit: waiting for a free slot");
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
    }

    /// Record that a request was made and the limits its response advertised.
    pub fn record(&self, endpoint: &str, limits: &[Bucket]) {
        let mut state = self.load();
        if !limits.is_empty() {
            state.limits.insert(endpoint.to_string(), limits.to_vec());
        }
        // Only times inside the longest window can still constrain a request.
        let horizon = state
            .limits
            .get(endpoint)
            .map(|buckets| buckets.iter().map(|b| b.window).fold(0.0, f64::max))
            .unwrap_or(0.0);
        let now = now_secs();
        let history = state.history.entry(endpoint.to_string()).or_default();
        history.push(now);
        history.retain(|&at| now - at < horizon);
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

/// How long to wait before one more request fits every bucket. Zero when a
/// window still has room, so a burst inside the limits is not slowed at all.
///
/// `history` is oldest-first unix times of prior requests to the endpoint.
fn delay_for(limits: &[Bucket], history: &[f64], now: f64, margin: f64) -> f64 {
    let mut delay: f64 = 0.0;
    for bucket in limits {
        if bucket.max == 0 || bucket.window <= 0.0 {
            continue;
        }
        let inside: Vec<f64> = history
            .iter()
            .copied()
            .filter(|&at| now - at < bucket.window)
            .collect();
        // Room to spare: this bucket imposes no wait.
        let Some(surplus) = inside.len().checked_sub(bucket.max as usize) else {
            continue;
        };
        // Enough of the oldest must age out to leave room for one more.
        let frees_up = inside[surplus] + bucket.window - now + margin;
        delay = delay.max(frees_up);
    }
    delay.max(0.0)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limits GGG advertises for trade search.
    fn trade_limits() -> Vec<Bucket> {
        [(5, 10.0), (15, 60.0), (30, 300.0), (600, 21600.0)]
            .map(|(max, window)| Bucket { max, window })
            .to_vec()
    }

    #[test]
    fn a_burst_inside_every_window_is_not_delayed() {
        let now = 1_000_000.0;
        // Four searches in the last two seconds: the 5-per-10s bucket still has
        // room, and the 6-hour budget is nowhere near spent.
        let history = vec![now - 2.0, now - 1.5, now - 1.0, now - 0.5];
        assert_eq!(delay_for(&trade_limits(), &history, now, 0.5), 0.0);
    }

    #[test]
    fn a_long_budget_does_not_pace_short_requests() {
        let now = 1_000_000.0;
        // 43 requests spread over the last hour — the state GGG reported. The
        // 600-per-6h bucket must not impose the 36s spacing this used to.
        let history: Vec<f64> = (0..43).map(|i| now - 3600.0 + i as f64 * 80.0).collect();
        assert_eq!(delay_for(&trade_limits(), &history, now, 0.5), 0.0);
    }

    #[test]
    fn a_full_window_waits_only_for_the_oldest_to_age_out() {
        let now = 1_000_000.0;
        // Five in the last 10s fills the tightest bucket; the oldest is 8s old,
        // so it leaves the window in 2s (plus the margin).
        let history = vec![now - 8.0, now - 6.0, now - 4.0, now - 2.0, now - 1.0];
        let delay = delay_for(&trade_limits(), &history, now, 0.5);
        assert!((delay - 2.5).abs() < 1e-9, "expected 2.5s, got {delay}");
    }

    #[test]
    fn the_most_constrained_window_decides() {
        let now = 1_000_000.0;
        // 15 requests in the last 30s: the 5-per-10s and 15-per-60s buckets are
        // both full. The minute bucket needs its oldest (30s old) to age out at
        // +30s, which outlasts the 10s bucket's wait.
        let history: Vec<f64> = (0..15).map(|i| now - 30.0 + i as f64 * 2.0).collect();
        let delay = delay_for(&trade_limits(), &history, now, 0.0);
        assert!((delay - 30.0).abs() < 1e-9, "expected 30s, got {delay}");
    }

    #[test]
    fn no_history_and_no_limits_never_wait() {
        let now = 1_000_000.0;
        assert_eq!(delay_for(&trade_limits(), &[], now, 0.5), 0.0);
        assert_eq!(delay_for(&[], &[now, now], now, 0.5), 0.0);
        // A malformed bucket is ignored rather than dividing by zero.
        let zero = [Bucket { max: 0, window: 10.0 }];
        assert_eq!(delay_for(&zero, &[now, now], now, 0.5), 0.0);
    }
}

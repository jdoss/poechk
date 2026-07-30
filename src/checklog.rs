//! Append-only JSONL record of every price check, for debugging after the fact.
//!
//! `poechk check` normally runs from a desktop shortcut, where stderr goes
//! nowhere the user can read it, so tracing alone cannot explain a bad check
//! once the overlay is gone. Every check appends the item it read, the exact
//! body sent to the trade API, and the raw responses to
//! `$XDG_STATE_HOME/poechk/checks.jsonl`, so a wrong price is diagnosed from
//! the log instead of reproduced.
//!
//! Lines from one process share a `check` id, so a session's events group with
//! `rg '"check":"<id>"' checks.jsonl`.
//!
//! Writing is best-effort: a failure is reported once and never fails the check
//! itself. POESESSID is never recorded, only whether one was sent. Listings do
//! carry seller account names, so treat the file as your own trade history.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Roll `checks.jsonl` over to `checks.jsonl.1` once it passes this size,
/// capping the log at twice this on disk. A fetch response runs a few hundred
/// KB, so this holds a few hundred searches — enough to still contain the check
/// that went wrong by the time anyone looks.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Handle to the check log. Cheap to construct: it holds only the path, and
/// each event opens, appends, and closes, so it needs no locking to share.
#[derive(Debug)]
pub struct CheckLog {
    /// `None` once logging has failed, which turns every event into a no-op.
    path: Option<PathBuf>,
}

impl CheckLog {
    /// Open the check log, rotating it first if it has grown past `MAX_BYTES`.
    pub fn open() -> Self {
        match prepare() {
            Ok(path) => Self { path: Some(path) },
            Err(e) => {
                warn_once(&format!("check log unavailable: {e}"));
                Self { path: None }
            }
        }
    }

    /// Append one event. `fields` must be a JSON object; its keys join `ts`,
    /// `check`, and `ev` on the line.
    pub fn event(&self, ev: &str, fields: Value) {
        let Some(path) = &self.path else {
            return;
        };
        let mut line = json!({ "ts": now_rfc3339(), "check": session(), "ev": ev });
        if let (Some(object), Value::Object(extra)) = (line.as_object_mut(), fields) {
            object.extend(extra);
        }
        if let Err(e) = append(path, &line) {
            warn_once(&format!("could not write {}: {e}", path.display()));
        }
    }
}

/// A response body as JSON when it parses, else the raw text. A rejected
/// request often answers with HTML or a Cloudflare page, which is exactly the
/// thing worth seeing verbatim.
pub fn body(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The plain-text sink that mirrors tracing output to disk. A check launched
/// from a desktop shortcut has no stderr anyone can read, so warnings about
/// rate-limit penalties and unparseable clipboards would otherwise be lost.
pub fn trace_file() -> Option<std::fs::File> {
    let path = dir().ok()?.join("poechk.log");
    rotate(&path).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// The log directory, created if absent. Logs are state, not cache: a wiped
/// cache is a non-event, a wiped log loses the check someone came to read.
fn dir() -> std::io::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "jdoss", "poechk")
        .ok_or_else(|| std::io::Error::other("could not locate a state directory"))?;
    let dir = dirs
        .state_dir()
        .unwrap_or_else(|| dirs.cache_dir())
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The log path, with its directory created and an oversized log rotated away.
fn prepare() -> std::io::Result<PathBuf> {
    let path = dir()?.join("checks.jsonl");
    rotate(&path)?;
    Ok(path)
}

/// Move an oversized log aside, replacing the previous rollover.
fn rotate(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_BYTES => std::fs::rename(path, rolled(path)),
        _ => Ok(()),
    }
}

/// The rollover path: `checks.jsonl` becomes `checks.jsonl.1`. Suffixed rather
/// than built with `with_extension`, which would turn `poechk.log` into
/// `poechk.1` and lose which log it came from.
fn rolled(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// Append one line. Written in a single call under `O_APPEND` so that two
/// overlapping checks interleave whole lines rather than corrupting each other.
fn append(path: &Path, line: &Value) -> std::io::Result<()> {
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Id shared by every line this process writes.
fn session() -> &'static str {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION.get_or_init(|| {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{millis:x}-{:x}", std::process::id())
    })
}

fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

/// Report a logging failure once, so a broken log cannot flood the real output.
fn warn_once(message: &str) {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        tracing::warn!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lines_carry_the_shared_frame_and_the_caller_fields() {
        let dir = std::env::temp_dir().join(format!("poechk-checklog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("checks.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = CheckLog {
            path: Some(path.clone()),
        };

        log.event("search_req", json!({ "url": "https://example/api", "body": { "a": 1 } }));
        log.event("search_resp", json!({ "status": 200 }));

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ev"], "search_req");
        assert_eq!(lines[0]["url"], "https://example/api");
        assert_eq!(lines[0]["body"]["a"], 1);
        assert_eq!(lines[1]["ev"], "search_resp");
        assert_eq!(lines[1]["status"], 200);
        // One process's lines group under a single id, and every line is stamped.
        assert_eq!(lines[0]["check"], lines[1]["check"]);
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_path_makes_events_a_no_op() {
        CheckLog { path: None }.event("search_req", json!({ "url": "x" }));
    }

    #[test]
    fn bodies_parse_as_json_but_survive_as_text_when_they_are_not() {
        assert_eq!(body(r#"{"total":47}"#)["total"], 47);
        assert_eq!(body("<html>403 Forbidden</html>"), "<html>403 Forbidden</html>");
        assert_eq!(body(""), "");
    }

    #[test]
    fn rotation_only_moves_a_log_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("poechk-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("checks.jsonl");
        let rolled = dir.join("checks.jsonl.1");
        let _ = std::fs::remove_file(&rolled);

        std::fs::write(&path, "small").unwrap();
        rotate(&path).unwrap();
        assert!(path.exists(), "a log under the cap stays put");
        assert!(!rolled.exists());

        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        rotate(&path).unwrap();
        assert!(!path.exists(), "an oversized log is moved aside");
        assert_eq!(std::fs::metadata(&rolled).unwrap().len(), MAX_BYTES + 1);

        std::fs::remove_file(&rolled).unwrap();
    }

    #[test]
    fn rotation_is_silent_when_there_is_no_log_yet() {
        rotate(&std::env::temp_dir().join("poechk-absent-log.jsonl")).unwrap();
    }

    #[test]
    fn rollover_names_keep_the_whole_original_filename() {
        assert_eq!(rolled(Path::new("/s/checks.jsonl")), Path::new("/s/checks.jsonl.1"));
        // A dotted extension must not swallow ".log" and collide across logs.
        assert_eq!(rolled(Path::new("/s/poechk.log")), Path::new("/s/poechk.log.1"));
    }
}

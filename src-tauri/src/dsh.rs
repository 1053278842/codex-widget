//! Watches DeepSeek Harness sessions and turns them into the same `ThreadView`
//! the Codex watcher produces.
//!
//! Two sources, used for different things:
//!
//! * **State** — `~/.dsh/storages/session_projcache/sessions/session-<id>.json`.
//!   A ~8 KB projection file the Host rewrites as the turn advances. `rows.turnBoundary.val.openTurnStartSeq`
//!   is non-null exactly while a turn is open, which is a far more robust
//!   signal than pairing start/end events.
//! * **Timestamps** — `<session>/session.v4.jsonl.zstd`. The projection keeps
//!   sequence numbers, not clock times, so the exact `turn/start` and `turn/end`
//!   times are read from the log. It is a stream of concatenated zstd frames and
//!   is only decoded when a turn opens or closes, then cached per turn.

use crate::watcher::ThreadView;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP_SECS: i64 = 24 * 3600;
const DROP_RUNNING_SECS: i64 = 6 * 3600;

#[derive(Default, Clone)]
struct TurnTime {
    start: Option<i64>,
    end: Option<i64>,
    interrupted: bool,
}

struct SessionState {
    /// value of the projection's seq, to notice "nothing changed"
    seq: i64,
    last_activity: i64,
    view: Option<ThreadView>,
    turns: HashMap<u64, TurnTime>,
    log_path: Option<PathBuf>,
}

pub struct DshWatcher {
    /// ~/.dsh/storages/session_projcache/sessions
    proj_dir: PathBuf,
    /// ~/.dsh/sessions
    log_root: PathBuf,
    sessions: HashMap<PathBuf, SessionState>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    Some(cursor)
}

fn int_at(value: &Value, path: &[&str]) -> Option<i64> {
    dig(value, path).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|n| n as i64))
            .or_else(|| v.as_f64().map(|n| n as i64))
    })
}

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    dig(value, path)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

impl DshWatcher {
    pub fn new(dsh_home: PathBuf) -> Self {
        Self {
            proj_dir: dsh_home
                .join("storages")
                .join("session_projcache")
                .join("sessions"),
            log_root: dsh_home.join("sessions"),
            sessions: HashMap::new(),
        }
    }

    /// Every turn worth showing, newest activity last.
    pub fn rows(&self) -> Vec<ThreadView> {
        let now = now_secs();
        self.sessions
            .values()
            .filter_map(|state| state.view.clone())
            .filter(|view| {
                if view.status == "running" {
                    now - view.last_activity < DROP_RUNNING_SECS
                } else {
                    now - view.completed_at.unwrap_or(view.last_activity) < KEEP_SECS
                }
            })
            .collect()
    }

    pub fn scan(&mut self) -> bool {
        let now = now_secs();
        let mut changed = false;

        let entries = match std::fs::read_dir(&self.proj_dir) {
            Ok(entries) => entries,
            Err(_) => return false,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let fresh = !self.sessions.contains_key(&path);

            let Some(text) = read_text(&path) else { continue };
            let Ok(json) = serde_json::from_str::<Value>(&text) else {
                continue;
            };

            let state = self.sessions.entry(path.clone()).or_insert_with(|| SessionState {
                seq: -1,
                last_activity: now,
                view: None,
                turns: HashMap::new(),
                log_path: None,
            });

            let seq = int_at(&json, &["record", "rows", "turnBoundary", "seq"]).unwrap_or(-1);
            let moved = fresh || seq != state.seq;
            if moved {
                state.seq = seq;
                state.last_activity = now;
            }

            if !moved && state.view.is_some() {
                continue;
            }
            changed = true;

            let open_turn = dig(&json, &["record", "rows", "turnBoundary", "val", "openTurnStartSeq"])
                .and_then(Value::as_u64);
            let last_turn = int_at(&json, &["record", "rows", "turnBoundary", "val", "lastTurn"])
                .unwrap_or(0)
                .max(0) as u64;

            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("dsh")
                .trim_start_matches("session-")
                .to_string();

            if open_turn.is_none() && last_turn == 0 {
                // nothing has ever run here
                state.view = None;
                continue;
            }

            if state.log_path.is_none() {
                state.log_path = find_log(&self.log_root, &session_id);
            }

            let turn = if open_turn.is_some() { last_turn } else { last_turn };
            let mut time = state.turns.get(&turn).cloned().unwrap_or_default();

            if let Some(log) = state.log_path.clone() {
                let need_start = time.start.is_none();
                let need_end = time.end.is_none();
                // a closed turn that we never saw end needs one decode; an open
                // turn only needs the start, which is already cached afterwards
                if need_start || (open_turn.is_none() && need_end) {
                    let filled = decode_turn_times(&log, turn);
                    if filled.start.is_some() || filled.end.is_some() {
                        time.start = time.start.or(filled.start);
                        time.end = time.end.or(filled.end);
                        time.interrupted = time.interrupted || filled.interrupted;
                        state.turns.insert(turn, time.clone());
                    }
                }
            }

            let cwd = str_at(&json, &["record", "identity", "cwd"]).unwrap_or_default();
            let title = str_at(&json, &["record", "rows", "title", "val"]).unwrap_or_default();
            let model = str_at(
                &json,
                &["record", "rows", "modelSelection", "val", "lastUsed", "model"],
            )
            .unwrap_or_default();

            let (status, started_at, completed_at) = if open_turn.is_some() {
                ("running", time.start.unwrap_or(state.last_activity), None)
            } else if let Some(end) = time.end {
                (
                    if time.interrupted { "aborted" } else { "done" },
                    time.start.unwrap_or(0),
                    Some(end),
                )
            } else {
                // closed but the log has not caught up yet; show it as done at
                // the last projection write rather than hiding a finished turn
                ("done", time.start.unwrap_or(0), Some(state.last_activity))
            };

            // without a start time there is no honest elapsed value to show
            if status != "running" && started_at == 0 {
                state.view = None;
                continue;
            }

            state.view = Some(ThreadView {
                source: "dsh".to_string(),
                thread_id: session_id,
                cwd,
                title,
                model,
                provider: "dsh".to_string(),
                status: status.to_string(),
                started_at,
                completed_at,
                last_activity: if status == "running" {
                    state.last_activity
                } else {
                    completed_at.unwrap_or(state.last_activity)
                },
                tokens: 0,
            });
        }

        changed
    }
}

fn read_text(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// `<dsh_home>/sessions/<workspace-slug>/session-<id>/session.v4.jsonl.zstd`
fn find_log(root: &Path, session_id: &str) -> Option<PathBuf> {
    let target = format!("session-{session_id}");
    let workspaces = std::fs::read_dir(root).ok()?;
    for workspace in workspaces.flatten() {
        let candidate = workspace.path().join(&target).join("session.v4.jsonl.zstd");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Decode the whole (multi-frame) log and pull this turn's start/end times.
/// Only called when a turn opens or closes, never on the polling path.
fn decode_turn_times(path: &Path, turn: u64) -> TurnTime {
    let mut bytes = Vec::new();
    let Ok(mut file) = File::open(path) else {
        return TurnTime::default();
    };
    if file.read_to_end(&mut bytes).is_err() {
        return TurnTime::default();
    }
    let Ok(raw) = zstd::stream::decode_all(&bytes[..]) else {
        return TurnTime::default();
    };
    let text = String::from_utf8_lossy(&raw);

    let mut result = TurnTime::default();
    for line in text.lines() {
        if !line.contains("\"turn/start\"") && !line.contains("\"turn/end\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let number = int_at(&value, &["data", "turn"]).unwrap_or(-1);
        if number < 0 || number as u64 != turn {
            continue;
        }
        let millis = int_at(&value, &["time"]).unwrap_or(0);
        let seconds = if millis > 1_000_000_000_000 {
            millis / 1000
        } else {
            millis
        };
        match kind {
            "turn/start" => result.start = Some(seconds),
            "turn/end" => {
                result.end = Some(seconds);
                let reason = str_at(&value, &["data", "reason", "kind"]).unwrap_or_default();
                result.interrupted = reason == "interrupted";
            }
            _ => {}
        }
    }
    result
}
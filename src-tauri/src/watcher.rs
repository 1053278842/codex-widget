//! Watches `~/.codex/sessions/**/rollout-*.jsonl` and turns it into a small,
//! stable view of "which Codex turns are alive right now".
//!
//! Design notes (these are the bits that were learned the hard way):
//!
//! * **Tail by byte offset, never by mtime.** On this machine a rollout file can
//!   grow by kilobytes while its `LastWriteTime` stays put, so file time is not
//!   a usable "did it change" signal. We track a byte offset per file instead.
//! * **A turn, not a session, is the unit.** One rollout file holds many turns;
//!   `task_started` / `task_complete` / `turn_aborted` bracket each one.
//! * **Initial state is read from both ends.** The metadata we want (cwd, model,
//!   provider, first prompt) is at the *head* of the file; the current turn
//!   status is at the *tail* — and the tail can be tens of megabytes in.
//! * **`last_activity` is observation time**, not the event's own timestamp, so
//!   no ISO-8601 parsing is needed: if bytes arrived since the last scan, the
//!   turn is alive.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HEAD_BYTES: u64 = 64 * 1024;
const TAIL_SCAN_CHUNK: usize = 256 * 1024;
const TAIL_SCAN_LIMIT: u64 = 32 * 1024 * 1024;
const KEEP_SECS: i64 = 24 * 3600;
const DROP_RUNNING_SECS: i64 = 6 * 3600;
const DAY_DIRS: usize = 4;

#[derive(Clone, Serialize)]
pub struct ThreadView {
    pub thread_id: String,
    pub cwd: String,
    pub title: String,
    pub model: String,
    pub provider: String,
    /// "running" | "done" | "aborted"
    pub status: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub last_activity: i64,
    pub tokens: u64,
}

#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub threads: Vec<ThreadView>,
    pub now: i64,
}

#[derive(Default)]
struct FileState {
    offset: u64,
    pending: Vec<u8>,
    view: Option<ThreadView>,
}

pub struct Watcher {
    root: PathBuf,
    files: HashMap<PathBuf, FileState>,
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

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    dig(value, path)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn int_at(value: &Value, path: &[&str]) -> Option<i64> {
    dig(value, path).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|n| n as i64))
            .or_else(|| v.as_f64().map(|n| n as i64))
    })
}

impl Watcher {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: HashMap::new(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = now_secs();
        let mut threads: Vec<ThreadView> = self
            .files
            .values()
            .filter_map(|state| state.view.clone())
            .filter(|view| {
                if view.status == "running" {
                    now - view.last_activity < DROP_RUNNING_SECS
                } else {
                    now - view.completed_at.unwrap_or(view.last_activity) < KEEP_SECS
                }
            })
            .collect();
        threads.sort_by_key(|t| t.started_at);
        Snapshot { threads, now }
    }

    /// One polling pass. Returns `true` when anything observable changed.
    pub fn scan(&mut self) -> bool {
        let now = now_secs();
        let mut changed = false;

        let paths = match self.recent_rollouts() {
            Some(paths) => paths,
            None => return false,
        };

        let mut seen: Vec<PathBuf> = Vec::with_capacity(paths.len());
        for path in paths {
            seen.push(path.clone());
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let fresh = !self.files.contains_key(&path);

            let state = self.files.entry(path.clone()).or_default();

            if fresh {
                state.view = initial_view(&path);
                state.offset = size;
                state.pending.clear();
                if let Some(view) = state.view.as_mut() {
                    if view.status == "running" {
                        view.last_activity = now;
                    }
                }
                changed = true;
                continue;
            }

            if size <= state.offset {
                continue;
            }

            let grew = read_delta(&path, state);
            if !grew {
                continue;
            }
            changed = true;
            if let Some(view) = state.view.as_mut() {
                view.last_activity = now;
            }
        }

        // forget files that disappeared
        let before = self.files.len();
        self.files.retain(|path, _| seen.iter().any(|p| p == path));
        if self.files.len() != before {
            changed = true;
        }

        changed
    }

    /// Newest day directories under `sessions/YYYY/MM/DD`, flattened to files.
    fn recent_rollouts(&self) -> Option<Vec<PathBuf>> {
        let mut days: Vec<PathBuf> = Vec::new();
        for year in std::fs::read_dir(&self.root).ok()?.flatten() {
            let year_path = year.path();
            if !year_path.is_dir() {
                continue;
            }
            for month in std::fs::read_dir(&year_path).into_iter().flatten().flatten() {
                let month_path = month.path();
                if !month_path.is_dir() {
                    continue;
                }
                for day in std::fs::read_dir(&month_path).into_iter().flatten().flatten() {
                    let day_path = day.path();
                    if day_path.is_dir() {
                        days.push(day_path);
                    }
                }
            }
        }
        days.sort();
        days.reverse();
        days.truncate(DAY_DIRS);

        let mut files = Vec::new();
        for day in days {
            if let Ok(entries) = std::fs::read_dir(&day) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        files.push(path);
                    }
                }
            }
        }
        Some(files)
    }
}

/* ------------------------------------------------------------------ reading */

fn read_head(path: &Path) -> Option<Vec<String>> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let take = size.min(HEAD_BYTES) as usize;
    let mut buf = vec![0u8; take];
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    Some(
        String::from_utf8_lossy(&buf)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// Walk backwards to the newest turn-bracketing event, so a turn that started
/// 20 MB ago is still classified correctly at startup.
fn last_marker(path: &Path) -> Option<(String, Value)> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut scanned: u64 = 0;
    let mut end = size;

    while end > 0 && scanned < TAIL_SCAN_LIMIT {
        let start = end.saturating_sub(TAIL_SCAN_CHUNK as u64);
        let take = (end - start) as usize;
        let mut buf = vec![0u8; take];
        file.seek(SeekFrom::Start(start)).ok()?;
        let read = file.read(&mut buf).ok()?;
        buf.truncate(read);

        let text = String::from_utf8_lossy(&buf);
        for line in text.lines().rev() {
            if line.contains("\"task_started\"")
                || line.contains("\"task_complete\"")
                || line.contains("\"turn_aborted\"")
            {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                    if let Some(kind) = payload.get("type").and_then(Value::as_str) {
                        return Some((kind.to_string(), payload));
                    }
                }
            }
        }

        scanned += take as u64;
        end = start;
    }
    None
}

fn initial_view(path: &Path) -> Option<ThreadView> {
    let mut view = ThreadView {
        thread_id: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string(),
        cwd: String::new(),
        title: String::new(),
        model: String::new(),
        provider: String::new(),
        status: "idle".to_string(),
        started_at: 0,
        completed_at: None,
        last_activity: now_secs(),
        tokens: 0,
    };
    let mut saw_meta = false;

    if let Some(lines) = read_head(path) {
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
            let payload = value.get("payload").cloned().unwrap_or(Value::Null);
            match kind {
                "session_meta" => {
                    saw_meta = true;
                    if let Some(id) = str_at(&payload, &["id"]).or_else(|| str_at(&payload, &["session_id"])) {
                        view.thread_id = id;
                    }
                    view.cwd = str_at(&payload, &["cwd"]).unwrap_or_default();
                    view.provider = str_at(&payload, &["model_provider"]).unwrap_or_default();
                    view.model = str_at(&payload, &["model"])
                        .or_else(|| str_at(&payload, &["base_instructions", "provenance", "model"]))
                        .unwrap_or_default();
                }
                "turn_context" => {
                    if view.model.is_empty() {
                        view.model = str_at(&payload, &["model"]).unwrap_or_default();
                    }
                }
                "event_msg" => {
                    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                        "user_message" => {
                            if view.title.is_empty() {
                                if let Some(message) = str_at(&payload, &["message"]) {
                                    view.title = message.lines().next().unwrap_or("").to_string();
                                }
                            }
                        }
                        "token_count" => {
                            if let Some(total) =
                                int_at(&payload, &["info", "total_token_usage", "total_tokens"])
                            {
                                view.tokens = total.max(0) as u64;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    if let Some((kind, payload)) = last_marker(path) {
        match kind.as_str() {
            "task_started" => {
                view.status = "running".to_string();
                view.started_at = int_at(&payload, &["started_at"]).unwrap_or_else(now_secs);
            }
            "task_complete" => {
                view.status = "done".to_string();
                view.started_at = int_at(&payload, &["started_at"]).unwrap_or(0);
                view.completed_at = int_at(&payload, &["completed_at"]);
            }
            "turn_aborted" => {
                view.status = "aborted".to_string();
                view.started_at = int_at(&payload, &["started_at"]).unwrap_or(0);
                view.completed_at = int_at(&payload, &["completed_at"]).or(Some(now_secs()));
            }
            _ => {}
        }
    }

    if !saw_meta && view.status == "idle" {
        return None;
    }
    Some(view)
}

/// Incremental read: seek to the stored offset, consume only new bytes, and keep
/// a half-written last line for the next pass.
fn read_delta(path: &Path, state: &mut FileState) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size <= state.offset {
        return false;
    }
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return false;
    }
    let take = (size - state.offset) as usize;
    let mut buf = vec![0u8; take];
    let read = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf.truncate(read);
    state.offset += read as u64;

    let mut combined = std::mem::take(&mut state.pending);
    combined.extend_from_slice(&buf);

    let mut lines: Vec<&[u8]> = combined.split(|b| *b == b'\n').collect();
    // the final element is either an empty tail or an incomplete line
    let remainder = lines.pop().unwrap_or(&[]).to_vec();
    state.pending = remainder;

    let mut touched = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if apply_event(state, &value) {
            touched = true;
        }
    }
    touched
}

/// Fold one JSONL record into the thread view. Returns whether it mattered.
fn apply_event(state: &mut FileState, value: &Value) -> bool {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    let now = now_secs();

    let Some(view) = state.view.as_mut() else {
        // a file that had no usable head can still start producing turns
        state.view = Some(ThreadView {
            thread_id: "unknown".to_string(),
            cwd: String::new(),
            title: String::new(),
            model: String::new(),
            provider: String::new(),
            status: "idle".to_string(),
            started_at: 0,
            completed_at: None,
            last_activity: now,
            tokens: 0,
        });
        return apply_event(state, value);
    };

    match kind {
        "session_meta" => {
            if let Some(id) = str_at(&payload, &["id"]).or_else(|| str_at(&payload, &["session_id"])) {
                view.thread_id = id;
            }
            if let Some(cwd) = str_at(&payload, &["cwd"]) {
                view.cwd = cwd;
            }
            if let Some(provider) = str_at(&payload, &["model_provider"]) {
                view.provider = provider;
            }
            if let Some(model) = str_at(&payload, &["model"])
                .or_else(|| str_at(&payload, &["base_instructions", "provenance", "model"]))
            {
                view.model = model;
            }
            true
        }
        "turn_context" => {
            if let Some(model) = str_at(&payload, &["model"]) {
                view.model = model;
            }
            true
        }
        "token_usage_record" => true,
        "event_msg" => match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "task_started" => {
                view.status = "running".to_string();
                view.started_at = int_at(&payload, &["started_at"]).unwrap_or(now);
                view.completed_at = None;
                view.last_activity = now;
                true
            }
            "task_complete" => {
                view.status = "done".to_string();
                if let Some(started) = int_at(&payload, &["started_at"]) {
                    view.started_at = started;
                }
                view.completed_at = int_at(&payload, &["completed_at"]).or(Some(now));
                view.last_activity = now;
                true
            }
            "turn_aborted" => {
                view.status = "aborted".to_string();
                if let Some(started) = int_at(&payload, &["started_at"]) {
                    view.started_at = started;
                }
                view.completed_at = int_at(&payload, &["completed_at"]).or(Some(now));
                view.last_activity = now;
                true
            }
            "user_message" => {
                if view.title.is_empty() {
                    if let Some(message) = str_at(&payload, &["message"]) {
                        view.title = message.lines().next().unwrap_or("").to_string();
                    }
                }
                true
            }
            "token_count" => {
                if let Some(total) = int_at(&payload, &["info", "total_token_usage", "total_tokens"]) {
                    view.tokens = total.max(0) as u64;
                }
                true
            }
            "agent_message" | "item_completed" | "mcp_tool_call_end" => true,
            _ => false,
        },
        _ => false,
    }
}
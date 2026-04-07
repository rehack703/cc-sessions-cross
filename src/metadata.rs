//! Session metadata storage (status, notes).
//!
//! Persisted to `~/.config/cc-sessions/metadata.json`:
//! ```json
//! {
//!   "session-uuid": { "status": "active" },
//!   "session-uuid2": { "status": "done" }
//! }
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Paused,
    Done,
}

impl Status {
    /// Cycle to next status: active → paused → done → (none)
    /// Returns None to clear the status.
    pub fn next(self) -> Option<Status> {
        match self {
            Status::Active => Some(Status::Paused),
            Status::Paused => Some(Status::Done),
            Status::Done => None,
        }
    }

    pub fn indicator(self) -> &'static str {
        match self {
            Status::Active => "●",
            Status::Paused => "◐",
            Status::Done => "✓",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Active => write!(f, "active"),
            Status::Paused => write!(f, "paused"),
            Status::Done => write!(f, "done"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MetadataStore {
    #[serde(flatten)]
    pub sessions: HashMap<String, SessionMeta>,
}

fn metadata_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".config").join("cc-sessions").join("metadata.json"))
}

pub fn load() -> MetadataStore {
    let Ok(path) = metadata_path() else {
        return MetadataStore::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return MetadataStore::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn save(store: &MetadataStore) -> Result<()> {
    let path = metadata_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(store)?;
    fs::write(&path, content).context("Failed to write metadata")?;
    Ok(())
}

pub fn get_status(store: &MetadataStore, session_id: &str) -> Option<Status> {
    store.sessions.get(session_id).and_then(|m| m.status)
}

pub fn set_status(store: &mut MetadataStore, session_id: &str, status: Option<Status>) {
    if let Some(status) = status {
        store
            .sessions
            .entry(session_id.to_string())
            .or_default()
            .status = Some(status);
    } else {
        // Remove status; if no other fields, remove entry
        if let Some(meta) = store.sessions.get_mut(session_id) {
            meta.status = None;
        }
        // Clean up empty entries
        store.sessions.retain(|_, v| v.status.is_some());
    }
}

/// Cycle status: None → active → paused → done → None
pub fn cycle_status(store: &mut MetadataStore, session_id: &str) -> Option<Status> {
    let current = get_status(store, session_id);
    let next = match current {
        None => Some(Status::Active),
        Some(s) => s.next(),
    };
    set_status(store, session_id, next);
    next
}

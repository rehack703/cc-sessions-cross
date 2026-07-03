//! Archive/trash storage for sessions.
//!
//! Live transcripts stay in Claude/Codex-owned directories. Archived and
//! trashed transcripts are moved under `~/.local/share/cc-sessions/` with a
//! sidecar file that records the original path so they can be restored.

use crate::session::{Session, SessionAgent, SessionSource, SessionStorage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Debug, Serialize, Deserialize)]
struct Sidecar {
    agent: String,
    storage: String,
    session_id: String,
    original_path: PathBuf,
    moved_at_unix: u64,
}

pub fn find_sessions(storage: SessionStorage, agent: Option<SessionAgent>) -> Result<Vec<Session>> {
    if storage.is_live() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for agent_kind in [SessionAgent::Claude, SessionAgent::Codex, SessionAgent::Pi] {
        if agent.is_some_and(|a| a != agent_kind) {
            continue;
        }

        let dir = storage_dir(storage, agent_kind)?;
        if !dir.exists() {
            continue;
        }

        for entry in WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
                continue;
            }
            let parsed = match agent_kind {
                SessionAgent::Claude => crate::claude_code::parse_session_file(
                    path.to_path_buf(),
                    &SessionSource::Local,
                    storage,
                ),
                SessionAgent::Codex => crate::codex::parse_session_file(path.to_path_buf(), storage),
                SessionAgent::Pi => crate::pi::parse_session_file(path.to_path_buf(), storage),
                SessionAgent::Hermes => None,
            };
            if let Some(session) = parsed {
                sessions.push(session);
            }
        }
    }

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

pub fn archive_session(session: &Session) -> Result<PathBuf> {
    move_live_session(session, SessionStorage::Archive)
}

pub fn trash_session(session: &Session) -> Result<PathBuf> {
    move_live_session(session, SessionStorage::Trash)
}

pub fn restore_session(session: &Session) -> Result<PathBuf> {
    if session.storage.is_live() {
        anyhow::bail!("Session is already live");
    }
    ensure_local(session)?;

    let sidecar_path = sidecar_path(&session.filepath);
    let sidecar = read_sidecar(&sidecar_path)?;
    if sidecar.session_id != session.id {
        anyhow::bail!("Archive sidecar does not match selected session");
    }
    if sidecar.original_path.exists() {
        anyhow::bail!(
            "Cannot restore: original path already exists: {}",
            sidecar.original_path.display()
        );
    }
    if let Some(parent) = sidecar.original_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    move_file(&session.filepath, &sidecar.original_path)?;
    let _ = fs::remove_file(sidecar_path);
    Ok(sidecar.original_path)
}

fn move_live_session(session: &Session, target: SessionStorage) -> Result<PathBuf> {
    if !session.storage.is_live() {
        anyhow::bail!("Only live sessions can be moved to {}", target.display_name());
    }
    ensure_local(session)?;

    let dir = storage_dir(target, session.agent)?;
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let dest = dir.join(format!("{}.jsonl", session.id));
    if dest.exists() {
        anyhow::bail!("Destination already exists: {}", dest.display());
    }

    let sidecar = Sidecar {
        agent: session.agent.display_name().to_string(),
        storage: target.display_name().to_string(),
        session_id: session.id.clone(),
        original_path: session.filepath.clone(),
        moved_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
    };
    let content = serde_json::to_string_pretty(&sidecar)?;
    fs::write(sidecar_path(&dest), content).context("Failed to write archive sidecar")?;
    move_file(&session.filepath, &dest)?;
    Ok(dest)
}

fn ensure_local(session: &Session) -> Result<()> {
    if !matches!(session.source, SessionSource::Local) {
        anyhow::bail!("Archive/trash only supports local sessions for now");
    }
    Ok(())
}

fn storage_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".local").join("share").join("cc-sessions"))
}

fn storage_dir(storage: SessionStorage, agent: SessionAgent) -> Result<PathBuf> {
    Ok(storage_root()?
        .join(storage.display_name())
        .join(agent.storage_dir()))
}

fn sidecar_path(path: &Path) -> PathBuf {
    path.with_extension("meta.json")
}

fn read_sidecar(path: &Path) -> Result<Sidecar> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read sidecar: {}", path.display()))?;
    serde_json::from_str(&content).context("Failed to parse archive sidecar")
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            fs::copy(from, to).with_context(|| {
                format!(
                    "Failed to copy {} to {} after rename failed: {}",
                    from.display(),
                    to.display(),
                    rename_err
                )
            })?;
            fs::remove_file(from)
                .with_context(|| format!("Failed to remove original {}", from.display()))?;
            Ok(())
        }
    }
}

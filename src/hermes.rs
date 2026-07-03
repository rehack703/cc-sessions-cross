//! Hermes Agent session storage handling.
//!
//! Hermes stores sessions in a SQLite database:
//! ```text
//! ~/.hermes/state.db
//! ```
//!
//! Sessions table: id, title, model, started_at, message_count, etc.
//! Messages table: session_id, role, content, timestamp

use crate::session::{Session, SessionAgent, SessionSource, SessionStorage};
use anyhow::{Context, Result};
use sqlite::State;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn get_hermes_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".hermes").join("state.db"))
}

pub fn find_sessions() -> Result<Vec<Session>> {
    let db_path = get_hermes_db_path()?;
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    find_sessions_in_db(&db_path, SessionStorage::Live)
}

pub fn find_sessions_in_db(db_path: &Path, storage: SessionStorage) -> Result<Vec<Session>> {
    let conn = sqlite::open(db_path)
        .with_context(|| format!("Failed to open Hermes database: {}", db_path.display()))?;

    let mut sessions = Vec::new();

    let query = "SELECT id, title, model, started_at, ended_at, message_count, 
                    input_tokens, output_tokens, estimated_cost_usd
             FROM sessions 
             ORDER BY started_at DESC";
    let mut statement = conn
        .prepare(query)
        .context("Failed to prepare sessions query")?;

    while let Ok(State::Row) = statement.next() {
        let id: String = statement.read::<String, _>("id").unwrap_or_default();
        let title: Option<String> = statement.read::<Option<String>, _>("title").unwrap_or(None);
        let model: Option<String> = statement.read::<Option<String>, _>("model").unwrap_or(None);
        let started_at: f64 = statement.read::<f64, _>("started_at").unwrap_or(0.0);
        let message_count: i64 = statement.read::<i64, _>("message_count").unwrap_or(0);

        let started = unix_timestamp_to_systemtime(started_at);

        let (first_message, summary) = get_session_summary(&conn, &id, &title, &model);

        let project = if let Some(ref t) = title {
            t.lines().next().unwrap_or("hermes").to_string()
        } else {
            "hermes".to_string()
        };

        let filepath = db_path.to_path_buf();

        sessions.push(Session {
            id,
            agent: SessionAgent::Hermes,
            project: truncate_project_name(&project),
            project_path: String::new(),
            filepath,
            created: started,
            modified: started,
            first_message: first_message.clone(),
            summary,
            name: title.clone(),
            tag: None,
            turn_count: message_count as usize,
            source: SessionSource::Local,
            storage,
            forked_from: None,
        });
    }

    Ok(sessions)
}

fn truncate_project_name(name: &str) -> String {
    let truncated: String = name.chars().take(60).collect();
    if truncated.len() < name.len() {
        truncated + "..."
    } else {
        truncated
    }
}

fn unix_timestamp_to_systemtime(ts: f64) -> SystemTime {
    let secs = ts.trunc() as u64;
    let nanos = (ts.fract() * 1_000_000_000.0) as u32;
    UNIX_EPOCH
        .checked_add(std::time::Duration::new(secs, nanos))
        .unwrap_or(UNIX_EPOCH)
}

fn get_session_summary(
    conn: &sqlite::Connection,
    session_id: &str,
    title: &Option<String>,
    model: &Option<String>,
) -> (Option<String>, Option<String>) {
    // Get first user message
    let first_msg = get_first_user_message(conn, session_id);
    // Use title as summary if available, otherwise model info
    let summary = title.clone().or_else(|| {
        model.as_ref().map(|m| format!("Model: {}", m))
    }).or_else(|| first_msg.clone());
    (first_msg, summary)
}

fn get_first_user_message(conn: &sqlite::Connection, session_id: &str) -> Option<String> {
    let mut statement = conn
        .prepare(
            "SELECT content FROM messages 
             WHERE session_id = ? AND role = 'user' AND content IS NOT NULL
             ORDER BY timestamp ASC LIMIT 1",
        )
        .ok()?;
    statement.bind((1, session_id)).ok()?;

    match statement.next() {
        Ok(State::Row) => {
            let content: String = statement.read::<String, _>("content").ok()?;
            let max_chars = 120;
            let truncated: String = content.chars().take(max_chars).collect();
            if truncated.len() < content.len() {
                Some(truncated + "...")
            } else {
                Some(truncated)
            }
        }
        _ => None,
    }
}

/// Build a preview from Hermes session messages.
pub fn generate_preview_content(_filepath: &Path) -> Result<String> {
    Ok(String::new())
}

/// Generate preview content for a Hermes session by session ID.
pub fn generate_preview_for_session_id(session_id: &str) -> Result<String> {
    let db_path = get_hermes_db_path()?;
    let conn = sqlite::open(&db_path)
        .with_context(|| format!("Failed to open Hermes database: {}", db_path.display()))?;

    let mut output = String::new();

    // Get session info
    let mut stmt = conn.prepare(
        "SELECT title, model, started_at, message_count, estimated_cost_usd 
         FROM sessions WHERE id = ?",
    )?;
    stmt.bind((1, session_id))?;

    if let Ok(State::Row) = stmt.next() {
        let title: Option<String> = stmt.read::<Option<String>, _>("title").unwrap_or(None);
        let model: Option<String> = stmt.read::<Option<String>, _>("model").unwrap_or(None);
        let started_at: f64 = stmt.read::<f64, _>("started_at").unwrap_or(0.0);
        let msg_count: i64 = stmt.read::<i64, _>("message_count").unwrap_or(0);
        let cost: Option<f64> = stmt.read::<Option<f64>, _>("estimated_cost_usd").unwrap_or(None);

        if let Some(ref m) = model {
            output.push_str(&format!("[Model: {}]\n", m));
        }
        if let Some(ref t) = title {
            output.push_str(&format!("[Title: {}]\n", t));
        }
        output.push_str(&format!("[Started: {}]\n", format_timestamp(started_at)));
        output.push_str(&format!("[Messages: {}]\n", msg_count));
        if let Some(c) = cost {
            output.push_str(&format!("[Cost: ${:.6}]\n", c));
        }
        output.push('\n');
    }

    // Get messages
    let mut msg_stmt = conn.prepare(
        "SELECT role, content FROM messages 
         WHERE session_id = ? AND content IS NOT NULL
         ORDER BY timestamp ASC",
    )?;
    msg_stmt.bind((1, session_id))?;

    while let Ok(State::Row) = msg_stmt.next() {
        let role: String = msg_stmt.read::<String, _>("role").unwrap_or_default();
        let content: String = msg_stmt.read::<String, _>("content").unwrap_or_default();

        let prefix = match role.as_str() {
            "user" => "\nUser: ",
            "assistant" => "\nAssistant: ",
            "tool" | "toolResult" => "\n  [Tool]",
            _ => "\nUnknown: ",
        };

        output.push_str(prefix);
        if role == "tool" || role == "toolResult" {
            if content.len() > 300 {
                let truncated: String = content.chars().take(300).collect();
                output.push_str(&truncated);
                output.push_str("...");
            } else {
                output.push_str(&content);
            }
        } else {
            output.push_str(&content);
            output.push('\n');
        }
    }

    Ok(output)
}

fn format_timestamp(ts: f64) -> String {
    let secs = ts.trunc() as i64;
    let datetime = chrono::DateTime::from_timestamp(secs, 0);
    match datetime {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => ts.to_string(),
    }
}

/// Generate search preview for Hermes session.
pub fn generate_search_preview(session_id: &str, pattern: &str) -> Result<String> {
    let content = generate_preview_for_session_id(session_id)?;
    let pattern_lower = pattern.to_ascii_lowercase();
    let mut output = String::new();

    for line in content.lines() {
        if line.to_ascii_lowercase().contains(&pattern_lower) {
            output.push_str(&format!("▶ {}\n", line));
        }
    }

    Ok(output)
}

/// Build search text index from Hermes session messages.
pub fn scan_search_text_for_session(session_id: &str) -> String {
    let db_path = match get_hermes_db_path() {
        Ok(p) => p,
        Err(_) => return String::new(),
    };
    let conn = match sqlite::open(&db_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut statement = match conn.prepare(
        "SELECT content FROM messages 
         WHERE session_id = ? AND content IS NOT NULL
         ORDER BY timestamp ASC",
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    if statement.bind((1, session_id)).is_err() {
        return String::new();
    }

    let mut out = String::new();
    while let Ok(State::Row) = statement.next() {
        if let Ok(content) = statement.read::<String, _>("content") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&content);
        }
    }

    out
}

/// Parse a Hermes session from the database by ID.
pub fn parse_session_by_id(session_id: &str, storage: SessionStorage) -> Option<Session> {
    let db_path = get_hermes_db_path().ok()?;
    let conn = sqlite::open(&db_path).ok()?;

    let mut statement = conn
        .prepare(
            "SELECT id, title, model, started_at, message_count 
             FROM sessions WHERE id = ?",
        )
        .ok()?;
    statement.bind((1, session_id)).ok()?;

    match statement.next() {
        Ok(State::Row) => {
            let id: String = statement.read::<String, _>("id").unwrap_or_default();
            let title: Option<String> = statement.read::<Option<String>, _>("title").unwrap_or(None);
            let model: Option<String> = statement.read::<Option<String>, _>("model").unwrap_or(None);
            let started_at: f64 = statement.read::<f64, _>("started_at").unwrap_or(0.0);
            let message_count: i64 = statement.read::<i64, _>("message_count").unwrap_or(0);

            let started = unix_timestamp_to_systemtime(started_at);
            let (first_message, summary) = get_session_summary(&conn, &id, &title, &model);

            let project = title
                .as_ref()
                .map(|t| truncate_project_name(t))
                .unwrap_or_else(|| "hermes".to_string());

            Some(Session {
                id,
                agent: SessionAgent::Hermes,
                project,
                project_path: String::new(),
                filepath: db_path,
                created: started,
                modified: started,
                first_message,
                summary,
                name: title,
                tag: None,
                turn_count: message_count as usize,
                source: SessionSource::Local,
                storage,
                forked_from: None,
            })
        }
        _ => None,
    }
}

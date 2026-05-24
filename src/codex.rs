//! Codex CLI session storage handling.
//!
//! Codex stores interactive transcripts under:
//!
//! ```text
//! ~/.codex/sessions/YYYY/MM/DD/rollout-...-<session-id>.jsonl
//! ```
//!
//! The `session_meta` event carries the stable session id and cwd. User-facing
//! conversation text is available from `event_msg:user_message` and assistant
//! `response_item:message` payloads.

use crate::session::{Session, SessionAgent, SessionSource, SessionStorage};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const BOLD_INVERSE: &str = "\x1b[1;7m";
const RESET: &str = "\x1b[0m";

pub fn get_codex_sessions_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".codex").join("sessions"))
}

pub fn find_sessions() -> Result<Vec<Session>> {
    let root = get_codex_sessions_dir()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    find_sessions_in_dir(&root, SessionStorage::Live)
}

pub fn find_sessions_in_dir(root: &Path, storage: SessionStorage) -> Result<Vec<Session>> {
    let jsonl_files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| is_session_file(e.path()))
        .map(|e| e.into_path())
        .collect();

    let sessions = jsonl_files
        .into_par_iter()
        .with_max_len(1)
        .filter_map(|path| parse_session_file(path, storage))
        .collect();

    Ok(sessions)
}

fn is_session_file(path: &Path) -> bool {
    path.extension() == Some(std::ffi::OsStr::new("jsonl"))
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with("rollout-") || s.len() >= 36)
            .unwrap_or(false)
}

pub fn parse_session_file(filepath: PathBuf, storage: SessionStorage) -> Option<Session> {
    let metadata = fs::metadata(&filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    let scan = scan_session_file(&filepath);
    let id = scan.id.or_else(|| id_from_filename(&filepath))?;
    let project_path = scan.project_path.unwrap_or_default();

    if project_path.is_empty() && scan.first_prompt.is_none() {
        return None;
    }

    Some(Session {
        id,
        agent: SessionAgent::Codex,
        project: project_name(&project_path),
        project_path,
        filepath,
        created,
        modified,
        first_message: scan.first_prompt,
        summary: None,
        name: None,
        tag: None,
        turn_count: scan.turn_count,
        source: SessionSource::Local,
        storage,
        forked_from: None,
    })
}

#[derive(Default)]
struct CodexScan {
    id: Option<String>,
    project_path: Option<String>,
    first_prompt: Option<String>,
    turn_count: usize,
}

fn scan_session_file(filepath: &Path) -> CodexScan {
    let mut scan = CodexScan::default();

    let Ok(file) = File::open(filepath) else {
        return scan;
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = String::new();

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if !line_mentions_codex_metadata(line.as_bytes()) {
            line.clear();
            continue;
        }

        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        match entry.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                let payload = entry.get("payload").unwrap_or(&Value::Null);
                if scan.id.is_none()
                    && let Some(id) = payload.get("id").and_then(|v| v.as_str())
                {
                    scan.id = Some(id.to_owned());
                }
                if scan.project_path.is_none()
                    && let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str())
                {
                    scan.project_path = Some(cwd.to_owned());
                }
            }
            Some("turn_context") => {
                if scan.project_path.is_none()
                    && let Some(cwd) = entry
                        .get("payload")
                        .and_then(|p| p.get("cwd"))
                        .and_then(|v| v.as_str())
                {
                    scan.project_path = Some(cwd.to_owned());
                }
            }
            Some("event_msg") => {
                let payload = entry.get("payload").unwrap_or(&Value::Null);
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message")
                    && let Some(message) = payload.get("message").and_then(|v| v.as_str())
                    && is_user_prompt(message)
                {
                    if scan.first_prompt.is_none() {
                        scan.first_prompt = Some(crate::normalize_summary(message, 120));
                    }
                    scan.turn_count += 1;
                }
            }
            _ => {}
        }
    }

    scan
}

fn line_mentions_codex_metadata(line: &[u8]) -> bool {
    line.windows(br#""type":"session_meta""#.len())
        .any(|w| w == br#""type":"session_meta""#)
        || line
            .windows(br#""type":"turn_context""#.len())
            .any(|w| w == br#""type":"turn_context""#)
        || line
            .windows(br#""type":"event_msg""#.len())
            .any(|w| w == br#""type":"event_msg""#)
}

fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() < 36 {
        return None;
    }
    let id = &stem[stem.len() - 36..];
    is_valid_uuid(id).then(|| id.to_owned())
}

fn is_valid_uuid(s: &str) -> bool {
    const DASH_POSITIONS: [usize; 4] = [8, 13, 18, 23];
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| {
            if DASH_POSITIONS.contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn project_name(project_path: &str) -> String {
    if project_path.is_empty() {
        return "unknown".to_string();
    }
    project_path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn is_user_prompt(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.starts_with("<environment_context>")
        && !trimmed.starts_with("<permissions")
        && !trimmed.starts_with("<collaboration_mode>")
}

#[derive(Debug)]
struct Message {
    role: &'static str,
    text: String,
}

pub fn generate_preview_content(filepath: &Path) -> Result<String> {
    let messages = read_messages(filepath, 100)?;
    if messages.is_empty() {
        return Ok("(empty session)".to_string());
    }

    let mut output = String::new();
    for msg in messages {
        let (glyph, color) = if msg.role == "user" {
            ('U', CYAN)
        } else {
            ('A', YELLOW)
        };
        let first_line = msg.text.lines().next().unwrap_or(&msg.text);
        output.push_str(&format!("{color}{glyph}: {first_line}{RESET}\n"));
    }
    Ok(output)
}

pub fn generate_search_preview(filepath: &Path, pattern: &str) -> Result<String> {
    let messages = read_messages(filepath, usize::MAX)?;
    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 10;

    output.push_str(&format!(
        "{GREEN}Searching for: \"{}\"{RESET}\n\n",
        pattern
    ));

    for (idx, msg) in messages.iter().enumerate() {
        if match_count >= MAX_MATCHES {
            output.push_str(&format!("\n{BOLD}... more matches truncated{RESET}\n"));
            break;
        }
        if !msg.text.to_lowercase().contains(&pattern_lower) {
            continue;
        }

        if match_count > 0 {
            output.push_str(&format!("\n{DIM}════════════════════════════════{RESET}\n\n"));
        }
        if idx > 0 {
            output.push_str(&format_context_message(&messages[idx - 1]));
            output.push('\n');
        }
        output.push_str(&format_matching_message(msg, pattern));
        if idx + 1 < messages.len() {
            output.push('\n');
            output.push_str(&format_context_message(&messages[idx + 1]));
        }
        match_count += 1;
    }

    if match_count == 0 {
        output.push_str("(no matches in transcript)");
    } else {
        output.push_str(&format!("\n\n{BOLD}{match_count} matching messages{RESET}"));
    }

    Ok(output)
}

pub fn scan_search_text(filepath: &Path) -> String {
    let Ok(messages) = read_messages(filepath, usize::MAX) else {
        return String::new();
    };
    let mut out = String::new();
    for msg in messages {
        if !out.is_empty() {
            out.push('\n');
        }
        let start = out.len();
        out.push_str(&msg.text);
        unsafe { out.as_bytes_mut()[start..].make_ascii_lowercase() };
    }
    out
}

fn read_messages(filepath: &Path, limit: usize) -> Result<Vec<Message>> {
    let file = File::open(filepath).context("Could not open Codex session file")?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut messages = Vec::new();
    let mut line = String::new();

    while messages.len() < limit && reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if !line_mentions_codex_message(line.as_bytes()) {
            line.clear();
            continue;
        }

        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        match entry.get("type").and_then(|v| v.as_str()) {
            Some("event_msg") => {
                let payload = entry.get("payload").unwrap_or(&Value::Null);
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message")
                    && let Some(message) = payload.get("message").and_then(|v| v.as_str())
                    && is_user_prompt(message)
                {
                    messages.push(Message {
                        role: "user",
                        text: message.to_owned(),
                    });
                }
            }
            Some("response_item") => {
                let payload = entry.get("payload").unwrap_or(&Value::Null);
                if payload.get("type").and_then(|v| v.as_str()) == Some("message")
                    && payload.get("role").and_then(|v| v.as_str()) == Some("assistant")
                    && let Some(text) = first_content_text(payload.get("content"))
                {
                    messages.push(Message {
                        role: "assistant",
                        text: text.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(messages)
}

fn line_mentions_codex_message(line: &[u8]) -> bool {
    line.windows(br#""type":"event_msg""#.len())
        .any(|w| w == br#""type":"event_msg""#)
        || (line
            .windows(br#""type":"response_item""#.len())
            .any(|w| w == br#""type":"response_item""#)
            && line
                .windows(br#""role":"assistant""#.len())
                .any(|w| w == br#""role":"assistant""#))
}

fn first_content_text(content: Option<&Value>) -> Option<&str> {
    let content = content?;
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("output_text"))
        .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
        .next()
}

fn format_context_message(msg: &Message) -> String {
    let prefix = if msg.role == "user" { "U" } else { "A" };
    let mut output = String::new();
    for (i, line) in msg.text.lines().take(10).enumerate() {
        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!("{DIM}{leader}{line}{RESET}\n"));
    }
    output
}

fn format_matching_message(msg: &Message, pattern: &str) -> String {
    let (prefix, color) = if msg.role == "user" {
        ("U", CYAN)
    } else {
        ("A", YELLOW)
    };

    let mut output = String::new();
    for (i, line) in msg.text.lines().enumerate() {
        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!("{color}{leader}{}{RESET}\n", highlight_match(line, pattern)));
    }
    output
}

fn highlight_match(text: &str, pattern: &str) -> String {
    if pattern.is_empty() {
        return text.to_owned();
    }
    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let mut result = String::with_capacity(text.len() + 16);
    let mut last = 0;

    for (i, _) in text_lower.match_indices(&pattern_lower) {
        if i < last || !text.is_char_boundary(i) {
            continue;
        }
        let end = i + pattern.len();
        if end > text.len() || !text.is_char_boundary(end) {
            continue;
        }
        result.push_str(&text[last..i]);
        result.push_str(BOLD_INVERSE);
        result.push_str(&text[i..end]);
        result.push_str(RESET);
        last = end;
    }
    result.push_str(&text[last..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(content: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path =
            tmp.path()
                .join("rollout-2026-05-24T15-24-54-019e58a8-716c-7223-b80f-646a8d35f2d8.jsonl");
        fs::write(&path, content).unwrap();
        (tmp, path)
    }

    #[test]
    fn parse_codex_session_metadata_and_prompt() {
        let (_tmp, path) = fixture(
            r#"{"type":"session_meta","payload":{"id":"019e58a8-716c-7223-b80f-646a8d35f2d8","cwd":"/home/daesik/project"}}"#
        );
        fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019e58a8-716c-7223-b80f-646a8d35f2d8","cwd":"/home/daesik/project"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"real task prompt","images":[]}}"#,
                "\n"
            ),
        )
        .unwrap();

        let session = parse_session_file(path, SessionStorage::Live).unwrap();
        assert_eq!(session.id, "019e58a8-716c-7223-b80f-646a8d35f2d8");
        assert_eq!(session.project, "project");
        assert_eq!(session.first_message.as_deref(), Some("real task prompt"));
        assert_eq!(session.turn_count, 1);
        assert_eq!(session.agent, SessionAgent::Codex);
    }

    #[test]
    fn filename_id_fallback_uses_uuid_suffix() {
        let (_tmp, path) = fixture(
            r#"{"type":"session_meta","payload":{"cwd":"/home/daesik/project"}}"#,
        );
        assert_eq!(
            id_from_filename(&path).as_deref(),
            Some("019e58a8-716c-7223-b80f-646a8d35f2d8")
        );
    }

    #[test]
    fn preview_excludes_environment_context() {
        let (_tmp, path) = fixture(
            concat!(
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"<environment_context>\n</environment_context>"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"visible prompt"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"visible answer"}]}}"#,
                "\n"
            ),
        );

        let preview = generate_preview_content(&path).unwrap();
        assert!(preview.contains("visible prompt"));
        assert!(preview.contains("visible answer"));
        assert!(!preview.contains("environment_context"));
    }
}

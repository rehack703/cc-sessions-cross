//! Pi agent session storage handling.
//!
//! Pi stores interactive transcripts under:
//!
//! ```text
//! ~/.pi/agent/sessions/--home-<user>--/
//!   <timestamp>_<uuid>.jsonl
//! ```
//!
//! Each JSONL file contains entries with `type` field:
//! - `session` - session metadata (id, cwd, timestamp)
//! - `message` - conversation turns (user, assistant, toolResult)

use crate::session::{Session, SessionAgent, SessionSource, SessionStorage};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub fn get_pi_sessions_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".pi").join("agent").join("sessions"))
}

pub fn find_sessions() -> Result<Vec<Session>> {
    let root = get_pi_sessions_dir()?;
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
    if path.extension() != Some(std::ffi::OsStr::new("jsonl")) {
        return false;
    }
    let name = match path.file_stem().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return false,
    };
    // Pi files: <timestamp>_<uuid>
    // e.g. 2026-07-03T12-18-42-956Z_019f27ea-bc8b-7d80-ba8f-4c5c21501e06
    name.contains('_') && uuid_in_filename(name)
}

fn uuid_in_filename(name: &str) -> bool {
    // Find a UUID-like segment after the underscore
    if let Some(idx) = name.rfind('_') {
        let after = &name[idx + 1..];
        after.len() == 36
            && after.chars().filter(|&c| c == '-').count() == 4
            && after.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    } else {
        false
    }
}

pub fn parse_session_file(filepath: PathBuf, storage: SessionStorage) -> Option<Session> {
    let id = extract_session_id(&filepath)?;
    let metadata = fs::metadata(&filepath).ok()?;
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let created = metadata.created().unwrap_or(modified);

    let (project, project_path, first_message, summary, turn_count) =
        scan_session_file(&filepath)?;

    let agent = SessionAgent::Pi;

    Some(Session {
        id,
        agent,
        project,
        project_path,
        filepath,
        created,
        modified,
        first_message,
        summary,
        name: None,
        tag: None,
        turn_count,
        source: SessionSource::Local,
        storage,
        forked_from: None,
    })
}

/// Extract session ID from filename: `<timestamp>_<uuid>.jsonl` → `<uuid>`
fn extract_session_id(filepath: &Path) -> Option<String> {
    let stem = filepath.file_stem()?.to_str()?;
    if let Some(idx) = stem.rfind('_') {
        Some(stem[idx + 1..].to_string())
    } else {
        None
    }
}

/// Scan a pi session file to extract metadata.
/// Returns (project, project_path, first_message, summary, turn_count).
fn scan_session_file(filepath: &Path) -> Option<(String, String, Option<String>, Option<String>, usize)> {
    let file = File::open(filepath).ok()?;
    let reader = BufReader::with_capacity(64 * 1024, file);

    let mut cwd = String::new();
    let mut first_prompt: Option<String> = None;
    let mut turn_count = 0usize;
    let mut last_assistant_text: Option<String> = None;

    for line in reader.lines() {
        let line = line.ok()?;
        let entry: Value = serde_json::from_str(&line).ok()?;

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session") => {
                // Extract cwd
                if let Some(cwd_val) = entry.get("cwd").and_then(|v| v.as_str()) {
                    cwd = cwd_val.to_string();
                }
                continue;
            }
            Some("message") => {
                let msg = entry.get("message")?;
                let role = msg.get("role")?.as_str()?;
                let content = msg.get("content")?;

                let text = extract_text_content(content);
                let text = text.as_deref().unwrap_or("");

                match role {
                    "user" => {
                        // Check if this is a tool result (has toolCallId in the message)
                        let is_tool_result = msg.get("toolCallId").is_some()
                            || msg.get("toolName").is_some();

                        if !is_tool_result && !text.is_empty() {
                            if first_prompt.is_none() {
                                first_prompt = Some(text.to_string());
                            }
                            turn_count += 1;
                        }
                    }
                    "assistant" => {
                        if !text.is_empty() {
                            last_assistant_text = Some(text.to_string());
                        }
                    }
                    _ => {}
                }
                continue;
            }
            _ => {}
        }
    }

    // Use the last assistant response as summary fallback
    let final_summary = last_assistant_text.map(|s| {
        let max_chars = 120;
        if s.chars().count() <= max_chars {
            s
        } else {
            s.chars().take(max_chars).collect::<String>() + "..."
        }
    });

    let project = if cwd.is_empty() {
        "unknown".to_string()
    } else {
        cwd.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string()
    };

    Some((project, cwd, first_prompt, final_summary, turn_count))
}

/// Extract text content from a message content field (string or array of blocks).
fn extract_text_content(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    if let Some(blocks) = content.as_array() {
        let mut texts: Vec<&str> = Vec::new();
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    texts.push(text);
                }
            }
        }
        if !texts.is_empty() {
            return Some(texts.join(" "));
        }
    }
    None
}

/// Generate preview content for a pi session file.
pub fn generate_preview_content(filepath: &Path) -> Result<String> {
    let file = File::open(filepath)
        .with_context(|| format!("Failed to open: {}", filepath.display()))?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    let mut output = String::new();

    for line in reader.lines() {
        let line = line?;
        let entry: Value = serde_json::from_str(&line).map_err(|e| {
            anyhow::anyhow!("Failed to parse line in {}: {}", filepath.display(), e)
        })?;

        let entry_type = entry.get("type").and_then(|v| v.as_str());

        match entry_type {
            Some("session") => {
                if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                    let _ = id;
                }
                if let Some(model) = entry.get("model").and_then(|v| v.as_str()) {
                    output.push_str(&format!("[Model: {}]\n", model));
                }
                if let Some(provider) = entry.get("provider").and_then(|v| v.as_str()) {
                    output.push_str(&format!("[Provider: {}]\n", provider));
                }
                if let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str()) {
                    output.push_str(&format!("[{}, cwd: {}]\n", "session", cwd));
                } else {
                    output.push_str("[session start]\n");
                }
            }
            Some("model_change") => {
                if let Some(model) = entry.get("modelId").and_then(|v| v.as_str()) {
                    output.push_str(&format!("[Model → {}]\n", model));
                }
            }
            Some("message") => {
                let msg = match entry.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
                let content = msg.get("content");

                let prefix = match role {
                    "user" => "\nUser: ",
                    "assistant" => "\nAssistant: ",
                    "toolResult" => "\n  [Tool Result]",
                    _ => "\nUnknown: ",
                };

                output.push_str(prefix);

                if let Some(text) = content.and_then(|c| extract_text_content(c)) {
                    // Truncate very long tool results
                    if role == "toolResult" && text.len() > 500 {
                        output.push_str(&text[..500]);
                        output.push_str("...");
                    } else {
                        output.push_str(&text);
                    }
                }

                if role == "assistant" {
                    output.push('\n');
                }
            }
            _ => {}
        }
    }

    Ok(output)
}

/// Generate a search preview (context lines around matched pattern).
pub fn generate_search_preview(filepath: &Path, pattern: &str) -> Result<String> {
    let content = generate_preview_content(filepath)?;
    let pattern_lower = pattern.to_ascii_lowercase();
    let mut output = String::new();
    let lines = content.lines().collect::<Vec<&str>>();

    // Find matching lines and show context
    let mut last_shown: isize = -10;
    for (i, line) in lines.iter().enumerate() {
        if line.to_ascii_lowercase().contains(&pattern_lower) {
            if i as isize > last_shown + 3 {
                if !output.is_empty() {
                    output.push_str("───\n");
                }
            }
            let start = i.saturating_sub(1);
            let end = (i + 2).min(lines.len());
            for j in start..end {
                if j as isize > last_shown {
                    if j == i {
                        output.push_str(&format!("▶ {}\n", lines[j]));
                    } else {
                        output.push_str(&format!("  {}\n", lines[j]));
                    }
                    last_shown = j as isize;
                }
            }
        }
    }

    Ok(output)
}

/// Build search text index from a pi session file.
pub fn scan_search_text(filepath: &Path) -> String {
    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let reader = BufReader::with_capacity(64 * 1024, file);
    let mut out = String::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let msg = match entry.get("message") {
            Some(m) => m,
            None => continue,
        };
        let content = match msg.get("content") {
            Some(c) => c,
            None => continue,
        };

        if let Some(text) = extract_text_content(content) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
        }
    }

    out.shrink_to_fit();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_session_file(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_extract_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create_session_file(
            tmp.path(),
            "2026-07-03T12-18-42-956Z_019f27ea-bc8b-7d80-ba8f-4c5c21501e06.jsonl",
            r#"{"type":"session","id":"019f27ea-bc8b-7d80-ba8f-4c5c21501e06"}"#,
        );
        let id = extract_session_id(&path);
        assert_eq!(
            id,
            Some("019f27ea-bc8b-7d80-ba8f-4c5c21501e06".to_string())
        );
    }

    #[test]
    fn test_is_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let valid = create_session_file(
            tmp.path(),
            "2026-07-03T12-18-42-956Z_019f27ea-bc8b-7d80-ba8f-4c5c21501e06.jsonl",
            "{}",
        );
        assert!(is_session_file(&valid));

        let invalid = create_session_file(tmp.path(), "not-a-session.txt", "{}");
        assert!(!is_session_file(&invalid));
    }

    #[test]
    fn test_scan_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create_session_file(
            tmp.path(),
            "2026-07-03T12-18-42-956Z_test-uuid.jsonl",
            r#"{"type":"session","version":3,"id":"test-uuid","timestamp":"2026-07-03T12:18:42.956Z","cwd":"/home/daesik/projects/my-project"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-07-03T12:18:46.235Z","message":{"role":"user","content":[{"type":"text","text":"Hello, how are you?"}]}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-07-03T12:18:47.704Z","message":{"role":"assistant","content":[{"type":"text","text":"I'm doing great!"}]}}
{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-07-03T12:18:56.645Z","message":{"role":"user","content":[{"type":"text","text":"What's the weather like?"}]}}
{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-07-03T12:19:01.390Z","message":{"role":"assistant","content":[{"type":"text","text":"It's sunny!"}]}}"#,
        );
        let (project, cwd, first, summary, turns) = scan_session_file(&path).unwrap();
        assert_eq!(project, "my-project");
        assert_eq!(cwd, "/home/daesik/projects/my-project");
        assert_eq!(first, Some("Hello, how are you?".to_string()));
        assert_eq!(turns, 2);
        assert!(summary.is_some());
    }

    #[test]
    fn test_find_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("--home-daesik--");
        fs::create_dir_all(&user_dir).unwrap();

        create_session_file(
            &user_dir,
            "2026-07-03T12-18-42-956Z_00000000-0000-0000-0000-000000000001.jsonl",
            r#"{"type":"session","id":"00000000-0000-0000-0000-000000000001","cwd":"/home/daesik/project-a"}
{"type":"message","id":"m1","message":{"role":"user","content":"hi"}}
{"type":"message","id":"m2","message":{"role":"assistant","content":"hello"}}"#,
        );

        let sessions = find_sessions_in_dir(tmp.path(), SessionStorage::Live).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project, "project-a");
        assert_eq!(sessions[0].agent, SessionAgent::Pi);
    }
}

mod archive;
mod claude_code;
mod codex;
mod interactive_state;
mod message_classification;
mod metadata;
mod remote;
mod session;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use interactive_state::{Action as StateAction, Effect as StateEffect, InteractiveState};
use session::{Session, SessionAgent, SessionSource, SessionStorage};
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::SystemTime;

// =============================================================================
// CLI Interface
// =============================================================================

#[derive(Parser)]
#[command(
    name = "cc-sessions",
    about = "List and resume Claude Code and Codex sessions across projects and machines"
)]
struct Args {
    // -------------------------------------------------------------------------
    // Mode
    // -------------------------------------------------------------------------
    /// List mode: print sessions as a table (no picker, no preview). Use without --list for interactive picker
    #[arg(long, help_heading = "Mode")]
    list: bool,

    /// Number of sessions to show [default: 15]. List only (ignored in interactive mode)
    #[arg(long, default_value = "15", help_heading = "Mode")]
    count: usize,

    // -------------------------------------------------------------------------
    // Interactive-only (ignored with --list)
    // -------------------------------------------------------------------------
    /// Fork session instead of resuming (creates new session ID). Interactive only; ignored with --list
    #[arg(long, help_heading = "Interactive only")]
    fork: bool,

    /// Show session ID prefixes and extra stats
    #[arg(long, help_heading = "Mode")]
    debug: bool,

    /// Which agent's sessions to show
    #[arg(long, value_enum, default_value = "all", help_heading = "Mode")]
    agent: AgentFilter,

    /// Browse archived sessions instead of live sessions
    #[arg(long, conflicts_with = "trash", help_heading = "Mode")]
    archive: bool,

    /// Browse trashed sessions instead of live sessions
    #[arg(long, conflicts_with = "archive", help_heading = "Mode")]
    trash: bool,

    // -------------------------------------------------------------------------
    // List-only
    // -------------------------------------------------------------------------
    /// Include forked sessions in the table. List only (interactive mode shows forks via → navigation)
    #[arg(long, help_heading = "List only")]
    include_forks: bool,

    // -------------------------------------------------------------------------
    // Filtering (both modes)
    // -------------------------------------------------------------------------
    /// Filter by project name (substring match, case-insensitive)
    #[arg(long, help_heading = "Filtering")]
    project: Option<String>,

    /// Minimum number of conversation turns (filters out one-shot sessions)
    #[arg(long, help_heading = "Filtering")]
    min_turns: Option<usize>,

    /// Filter to sessions from a specific remote (e.g. devbox) or "local"
    #[arg(long, value_name = "NAME", help_heading = "Filtering")]
    remote: Option<String>,

    /// Filter by status (active, paused, done)
    #[arg(long, value_name = "STATUS", help_heading = "Filtering")]
    status: Option<String>,

    /// Include sessions marked done in the default live view
    #[arg(long, help_heading = "Filtering")]
    include_done: bool,

    // -------------------------------------------------------------------------
    // Remote sync
    // -------------------------------------------------------------------------
    /// Force sync all remotes before listing
    #[arg(long, help_heading = "Remote sync")]
    sync: bool,

    /// Skip auto-sync (use cached remote data only)
    #[arg(long, help_heading = "Remote sync")]
    no_sync: bool,

    /// Sync all remotes and exit; no listing or picker (e.g. for cron). Other flags ignored
    #[arg(long, help_heading = "Remote sync")]
    sync_only: bool,

    /// Treat any remote sync/discovery source failure as fatal
    #[arg(long, help_heading = "Remote sync")]
    strict: bool,

    // -------------------------------------------------------------------------
    // Internal (hidden from --help)
    // -------------------------------------------------------------------------
    /// Preview a session file (used internally by interactive picker)
    #[arg(long, value_name = "FILE", hide = true)]
    preview: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AgentFilter {
    All,
    Claude,
    Codex,
}

impl AgentFilter {
    fn includes(self, agent: SessionAgent) -> bool {
        match self {
            AgentFilter::All => true,
            AgentFilter::Claude => agent == SessionAgent::Claude,
            AgentFilter::Codex => agent == SessionAgent::Codex,
        }
    }

    fn as_agent(self) -> Option<SessionAgent> {
        match self {
            AgentFilter::All => None,
            AgentFilter::Claude => Some(SessionAgent::Claude),
            AgentFilter::Codex => Some(SessionAgent::Codex),
        }
    }
}

// =============================================================================
// Main Entry Point
// =============================================================================

fn main() -> Result<()> {
    let args = Args::parse();

    // Preview mode: output formatted transcript for a session file
    if let Some(ref filepath) = args.preview {
        print_session_preview(filepath)?;
        return Ok(());
    }

    // Load remote config
    let config = remote::load_config()?;

    // Handle sync operations
    if args.sync_only {
        // Sync all remotes and exit
        let summary = remote::sync_all(&config)?;
        for result in &summary.successes {
            println!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        for failure in &summary.failures {
            eprintln!(
                "Warning: Failed to sync '{}': {}",
                failure.remote_name, failure.reason
            );
        }
        if summary.successes.is_empty() {
            println!("No remotes configured. Add remotes to ~/.config/cc-sessions/remotes.toml");
        }
        enforce_strict_mode(args.strict, summary.failure_count(), 0)?;
        return Ok(());
    }

    let mut sync_failures = 0;

    if args.sync {
        // Force sync all remotes
        let summary = remote::sync_all(&config)?;
        for result in &summary.successes {
            eprintln!(
                "Synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        sync_failures = summary.failure_count();
    } else if !args.no_sync && !config.remotes.is_empty() {
        // Auto-sync stale remotes
        let summary = remote::sync_if_stale(&config)?;
        for result in &summary.successes {
            eprintln!(
                "Auto-synced '{}' in {:.1}s",
                result.remote_name,
                result.duration.as_secs_f64()
            );
        }
        sync_failures = summary.failure_count();
    }

    let storage_view = if args.archive {
        SessionStorage::Archive
    } else if args.trash {
        SessionStorage::Trash
    } else {
        SessionStorage::Live
    };

    let mut discovery_failures = 0;
    let mut sessions = if storage_view.is_live() {
        // Find sessions from all live sources.
        let mut sessions = Vec::new();

        if args.agent.includes(SessionAgent::Claude) {
            let discovery =
                claude_code::find_all_sessions_with_summary(&config, args.remote.as_deref())?;
            for failure in &discovery.failures {
                eprintln!(
                    "Warning: Failed to load sessions from '{}': {}",
                    failure.source_name, failure.reason
                );
            }
            discovery_failures += discovery.failure_count();
            sessions.extend(discovery.sessions);
        }

        if args.agent.includes(SessionAgent::Codex) && args.remote.is_none() {
            sessions.extend(codex::find_sessions()?);
        }

        sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
        sessions
    } else {
        archive::find_sessions(storage_view, args.agent.as_agent())?
    };

    enforce_strict_mode(args.strict, sync_failures, discovery_failures)?;

    // Filter by project name if specified
    if let Some(ref filter) = args.project {
        let filter_lower = filter.to_lowercase();
        sessions.retain(|s| s.project.to_lowercase().contains(&filter_lower));
    }

    // Filter by minimum turns (excludes one-shot sessions)
    if let Some(min) = args.min_turns {
        sessions.retain(|s| s.turn_count >= min);
    }

    // Filter by status
    if let Some(ref status_filter) = args.status {
        let meta_store = metadata::load();
        let filter_lower = status_filter.to_lowercase();
        sessions.retain(|s| {
            let status = metadata::get_status(&meta_store, &s.id);
            match filter_lower.as_str() {
                "active" => status == Some(metadata::Status::Active),
                "paused" => status == Some(metadata::Status::Paused),
                "done" => status == Some(metadata::Status::Done),
                "none" => status.is_none(),
                _ => true,
            }
        });
    } else if !args.include_done && storage_view.is_live() {
        let meta_store = metadata::load();
        sessions
            .retain(|s| metadata::get_status(&meta_store, &s.id) != Some(metadata::Status::Done));
    }

    if sessions.is_empty() {
        if args.project.is_some() {
            anyhow::bail!("No sessions found matching project filter");
        }
        if let Some(ref remote_name) = args.remote {
            anyhow::bail!("No sessions found for remote '{}'", remote_name);
        }
        anyhow::bail!("No {} sessions found", storage_view.display_name());
    }

    if args.list {
        let list_sessions = filter_forks_for_list(&sessions, args.include_forks);
        print_sessions(&list_sessions, args.count, args.debug);
    } else {
        interactive_mode(sessions, args.fork, args.debug)?;
    }

    Ok(())
}

fn enforce_strict_mode(
    strict: bool,
    sync_failures: usize,
    discovery_failures: usize,
) -> Result<()> {
    if !strict {
        return Ok(());
    }

    if sync_failures > 0 {
        anyhow::bail!("Strict mode: {} sync source(s) failed", sync_failures);
    }

    if discovery_failures > 0 {
        anyhow::bail!(
            "Strict mode: {} discovery source(s) failed",
            discovery_failures
        );
    }

    Ok(())
}

// =============================================================================
// Display Functions
// =============================================================================

fn print_sessions(sessions: &[&Session], count: usize, debug: bool) {
    let meta_store = metadata::load();

    if debug {
        println!(
            "{:<6} {:<6} {:<2} {:<4} {:<6} {:<8} {:<16} {:<40} SUMMARY",
            "CREAT", "MOD", "ST", "FORK", "AGENT", "SOURCE", "PROJECT", "ID"
        );
        println!("{}", "─".repeat(142));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let agent = session.agent.display_name();
            let source = session.source.display_name();
            let st = metadata::get_status(&meta_store, &session.id)
                .map(|s| s.indicator())
                .unwrap_or(" ");
            let fork_indicator = if session.forked_from.is_some() {
                "↳"
            } else {
                ""
            };
            let id_short = if session.id.len() > 36 {
                &session.id[..36]
            } else {
                &session.id
            };
            let desc = format_session_desc(session, 30);
            let desc = if session.name.is_some() {
                format!("{}{}{}", colors::YELLOW, desc, colors::RESET)
            } else {
                desc
            };

            println!(
                "{:<6} {:<6} {:<2} {:<4} {:<6} {:<8} {:<16} {:<40} {}",
                created, modified, st, fork_indicator, agent, source, session.project, id_short, desc
            );
        }

        println!("{}", "─".repeat(142));
        println!("Total: {} sessions", sessions.len());
    } else {
        println!(
            "{:<6} {:<6} {:<2} {:<6} {:<8} {:<16} SUMMARY",
            "CREAT", "MOD", "ST", "AGENT", "SOURCE", "PROJECT"
        );
        println!("{}", "─".repeat(112));

        for session in sessions.iter().take(count) {
            let created = format_time_relative(session.created);
            let modified = format_time_relative(session.modified);
            let agent = session.agent.display_name();
            let source = session.source.display_name();
            let st = metadata::get_status(&meta_store, &session.id)
                .map(|s| s.indicator())
                .unwrap_or(" ");
            let desc = format_session_desc(session, 50);
            let desc = if session.forked_from.is_some() {
                format!("↳ {}", desc)
            } else {
                desc
            };
            let desc = if session.name.is_some() {
                format!("{}{}{}", colors::YELLOW, desc, colors::RESET)
            } else {
                desc
            };

            println!(
                "{:<6} {:<6} {:<2} {:<6} {:<8} {:<16} {}",
                created, modified, st, agent, source, session.project, desc
            );
        }

        println!("{}", "─".repeat(112));
        println!("Run without --list for interactive picker; use --fork to fork when resuming");
        println!("Filter by agent/status: --agent claude|codex|all --status active|paused|done");
    }
}

fn format_time_relative(time: SystemTime) -> String {
    let now = SystemTime::now();

    // Handle future timestamps (clock skew, filesystem issues)
    let secs = match now.duration_since(time) {
        Ok(d) => d.as_secs(),
        Err(_) => return "?".to_string(), // Future timestamp
    };

    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 604800 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / 604800)
    }
}

/// Format session description: name (★) > tag (#) > summary > first_message
fn format_session_desc(session: &Session, max_chars: usize) -> String {
    let label = match (&session.name, &session.tag) {
        (Some(name), Some(tag)) => Some(format!("★ {} #{}", name, tag)),
        (Some(name), None) => Some(format!("★ {}", name)),
        (None, Some(tag)) => Some(format!("#{}", tag)),
        (None, None) => None,
    };

    if let Some(label) = label {
        let label_len = label.chars().count();
        if label_len >= max_chars {
            return label.chars().take(max_chars).collect();
        }
        // Append summary if there's room for " - " + at least 10 chars
        if let Some(summary) = &session.summary
            && max_chars > label_len + 13
        {
            let remaining = max_chars - label_len - 3;
            return format!(
                "{} - {}",
                label,
                summary.chars().take(remaining).collect::<String>()
            );
        }
        return label;
    }

    session
        .summary
        .as_deref()
        .or(session.first_message.as_deref())
        .map(|s| s.chars().take(max_chars).collect())
        .unwrap_or_default()
}

fn filter_forks_for_list(sessions: &[Session], include_forks: bool) -> Vec<&Session> {
    if include_forks {
        return sessions.iter().collect();
    }

    sessions
        .iter()
        .filter(|s| s.forked_from.is_none())
        .collect()
}

/// Normalize text for display: collapse whitespace, strip markdown, truncate gracefully
pub fn normalize_summary(text: &str, max_chars: usize) -> String {
    // Collapse whitespace and build directly into the output buffer — stop
    // collecting once we're past max_chars (summary inputs can be very long).
    let mut normalized = String::with_capacity(max_chars.min(text.len()) + 4);
    let mut words = text.split_whitespace();
    if let Some(first) = words.next() {
        normalized.push_str(first);
        for w in words {
            normalized.push(' ');
            normalized.push_str(w);
            if normalized.len() > max_chars * 4 {
                break;
            }
        }
    }

    let stripped = normalized.trim_start_matches(['#', '*']).trim_start();

    if stripped.chars().count() <= max_chars {
        return stripped.to_owned();
    }

    let truncated: String = stripped.chars().take(max_chars).collect();
    let break_point = truncated
        .rfind(' ')
        .filter(|&i| i > max_chars / 2)
        .unwrap_or(truncated.len());

    format!("{}...", &truncated[..break_point])
}

// =============================================================================
// ANSI Colors (shared across preview functions)
// =============================================================================

mod colors {
    pub const CYAN: &str = "\x1b[36m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const GREEN: &str = "\x1b[32m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const BOLD_INVERSE: &str = "\x1b[1;7m";
    pub const RESET: &str = "\x1b[0m";
}

// =============================================================================
// Preview Mode (internal, replaces jaq dependency)
// =============================================================================

/// Print formatted transcript preview for a session file.
/// Used internally by skim's preview command.
fn print_session_preview(filepath: &PathBuf) -> Result<()> {
    let content = if looks_like_codex_path(filepath) {
        codex::generate_preview_content(filepath)?
    } else {
        generate_preview_content(filepath)?
    };
    print!("{}", content);
    Ok(())
}

fn looks_like_codex_path(path: &std::path::Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy() == ".codex")
        || path
            .components()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w[0].as_os_str().to_string_lossy() == "codex")
}

fn generate_preview_for_session(session: &Session) -> Result<String> {
    match session.agent {
        SessionAgent::Claude => generate_preview_content(&session.filepath),
        SessionAgent::Codex => codex::generate_preview_content(&session.filepath),
    }
}

fn generate_search_preview_for_session(session: &Session, pattern: &str) -> Result<String> {
    match session.agent {
        SessionAgent::Claude => generate_search_preview(&session.filepath, pattern),
        SessionAgent::Codex => codex::generate_search_preview(&session.filepath, pattern),
    }
}

fn build_search_index_for_sessions(
    targets: Vec<(String, SessionAgent, PathBuf)>,
) -> claude_code::SearchIndex {
    targets
        .into_iter()
        .map(|(id, agent, path)| {
            let text = match agent {
                SessionAgent::Claude => claude_code::scan_search_text(&path),
                SessionAgent::Codex => codex::scan_search_text(&path),
            };
            (id, text)
        })
        .collect()
}

/// Extract first text block from a message entry, borrowing from the JSON value
fn extract_message_text(entry: &serde_json::Value) -> Option<&str> {
    let content = entry.get("message")?.get("content")?;
    claude_code::first_text_block(content)
}

/// Generate preview content as a string (for skim's preview pane). Skim is
/// configured with `:wrap`, so we emit untruncated lines and let the pane
/// handle overflow — no arbitrary width caps.
fn generate_preview_content(filepath: &PathBuf) -> Result<String> {
    use std::fmt::Write as _;
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(filepath).context("Could not open session file")?;
    let mut reader = BufReader::new(file);

    let mut output = String::new();
    let mut line = String::new();
    let mut line_count = 0;
    const MAX_LINES: usize = 100;

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if line_count >= MAX_LINES {
            break;
        }
        if !claude_code::line_mentions_content_type(line.as_bytes()) {
            line.clear();
            continue;
        }

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        let (role_glyph, color) = match entry.get("type").and_then(|v| v.as_str()) {
            Some("user") => ('U', colors::CYAN),
            Some("assistant") => ('A', colors::YELLOW),
            _ => continue,
        };

        let Some(text) = extract_message_text(&entry) else {
            continue;
        };
        if role_glyph == 'U' && is_system_content(text) {
            continue;
        }

        let first_line = text.lines().next().unwrap_or(text);
        let _ = writeln!(output, "{color}{role_glyph}: {first_line}{}", colors::RESET);
        line_count += 1;
    }

    if output.is_empty() {
        output.push_str("(empty session)");
    }

    Ok(output)
}

/// Check if content is system/XML content that should be skipped in previews
fn is_system_content(text: &str) -> bool {
    message_classification::is_system_content_for_preview(text)
}

/// A message from the transcript
struct Message {
    role: String, // "user" or "assistant"
    text: String,
}

/// Generate preview showing matching messages with full conversation context
fn generate_search_preview(filepath: &PathBuf, pattern: &str) -> Result<String> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(filepath).context("Could not open session file")?;
    let mut reader = BufReader::new(file);

    // Collect all messages first (filter out progress/attachment lines before
    // the JSON parse — large sessions are dominated by those).
    let mut messages: Vec<Message> = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        if !claude_code::line_mentions_content_type(line.as_bytes()) {
            line.clear();
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        let role = match entry.get("type").and_then(|v| v.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        if let Some(text) = extract_message_text(&entry) {
            if role == "user" && is_system_content(text) {
                continue;
            }
            messages.push(Message {
                role: role.to_owned(),
                text: text.to_owned(),
            });
        }
    }

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();
    let mut match_count = 0;
    const MAX_MATCHES: usize = 10; // Fewer matches since we show full context

    output.push_str(&format!(
        "{}Searching for: \"{}\"{}\n\n",
        colors::GREEN,
        pattern,
        colors::RESET
    ));

    // Find messages containing the pattern
    let matching_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.text.to_lowercase().contains(&pattern_lower))
        .map(|(i, _)| i)
        .collect();

    // Show each match with surrounding context
    let mut shown_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for &match_idx in &matching_indices {
        if match_count >= MAX_MATCHES {
            output.push_str(&format!(
                "\n{}... more matches truncated{}\n",
                colors::BOLD,
                colors::RESET
            ));
            break;
        }

        // Skip if we already showed this message as context
        if shown_indices.contains(&match_idx) {
            continue;
        }

        // Separator between match groups
        if match_count > 0 {
            output.push_str(&format!(
                "\n{}════════════════════════════════{}\n\n",
                colors::DIM,
                colors::RESET
            ));
        }

        // Show previous message (context)
        if match_idx > 0 && !shown_indices.contains(&(match_idx - 1)) {
            let prev = &messages[match_idx - 1];
            output.push_str(&format_context_message(prev));
            output.push('\n');
            shown_indices.insert(match_idx - 1);
        }

        // Show matching message (highlighted)
        let msg = &messages[match_idx];
        output.push_str(&format_matching_message(msg, pattern));
        shown_indices.insert(match_idx);
        match_count += 1;

        // Show next message (context)
        if match_idx + 1 < messages.len() && !shown_indices.contains(&(match_idx + 1)) {
            output.push('\n');
            let next = &messages[match_idx + 1];
            output.push_str(&format_context_message(next));
            shown_indices.insert(match_idx + 1);
        }
    }

    if match_count == 0 {
        output.push_str("(no matches in transcript)");
    } else {
        output.push_str(&format!(
            "\n\n{}{} matching messages{}",
            colors::BOLD,
            match_count,
            colors::RESET
        ));
    }

    Ok(output)
}

/// Format a context message (dimmed, truncated if too long)
fn format_context_message(msg: &Message) -> String {
    let prefix = if msg.role == "user" { "U" } else { "A" };
    const MAX_CONTEXT_LINES: usize = 10;
    let lines: Vec<&str> = msg.text.lines().collect();

    let mut output = String::new();
    for (i, line) in lines.iter().take(MAX_CONTEXT_LINES).enumerate() {
        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!(
            "{}{}{}{}\n",
            colors::DIM,
            leader,
            line,
            colors::RESET
        ));
    }
    if lines.len() > MAX_CONTEXT_LINES {
        output.push_str(&format!(
            "{}   ... ({} more lines){}\n",
            colors::DIM,
            lines.len() - MAX_CONTEXT_LINES,
            colors::RESET
        ));
    }
    output
}

/// Format a matching message (colored, with highlights)
fn format_matching_message(msg: &Message, pattern: &str) -> String {
    let (prefix, color) = if msg.role == "user" {
        ("U", colors::CYAN)
    } else {
        ("A", colors::YELLOW)
    };

    let pattern_lower = pattern.to_lowercase();
    let mut output = String::new();

    for (i, line) in msg.text.lines().enumerate() {
        let formatted_line = if line.to_lowercase().contains(&pattern_lower) {
            highlight_match(line, pattern)
        } else {
            line.to_string()
        };

        let leader = if i == 0 {
            format!("{}: ", prefix)
        } else {
            "   ".to_string()
        };
        output.push_str(&format!(
            "{}{}{}{}\n",
            color,
            leader,
            formatted_line,
            colors::RESET
        ));
    }
    output
}

/// Highlight matching text with bold/inverse (Unicode-safe)
fn highlight_match(text: &str, pattern: &str) -> String {
    if pattern.is_empty() {
        return text.to_owned();
    }

    // Fast path: ASCII-only text and pattern. Lowercasing preserves byte
    // positions, so we lower once and match_indices gives us offsets directly.
    // This is O(n) vs. the generic path's per-position re-lowering.
    if text.is_ascii() && pattern.is_ascii() {
        let text_lower = text.to_ascii_lowercase();
        let pattern_lower = pattern.to_ascii_lowercase();
        let mut result = String::with_capacity(text.len() + 16);
        let mut last = 0;
        for (i, _) in text_lower.match_indices(&pattern_lower) {
            result.push_str(&text[last..i]);
            result.push_str(colors::BOLD_INVERSE);
            result.push_str(&text[i..i + pattern.len()]);
            result.push_str(colors::RESET);
            last = i + pattern.len();
        }
        result.push_str(&text[last..]);
        return result;
    }

    // Generic path: handles case-fold expansion (ß → ss, İ → i̇). Walk the
    // original by char, lower only the pattern-sized window at each position.
    let pattern_lower = pattern.to_lowercase();
    let pattern_char_count = pattern.chars().count();
    let mut result = String::with_capacity(text.len() + 16);
    let mut last_end = 0;

    let indices: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();

    let mut i = 0;
    while i + pattern_char_count < indices.len() {
        let start = indices[i];
        let end = indices[i + pattern_char_count];
        if text[start..end].to_lowercase() == pattern_lower {
            result.push_str(&text[last_end..start]);
            result.push_str(colors::BOLD_INVERSE);
            result.push_str(&text[start..end]);
            result.push_str(colors::RESET);
            last_end = end;
            i += pattern_char_count;
        } else {
            i += 1;
        }
    }
    result.push_str(&text[last_end..]);
    result
}

// =============================================================================
// Session Resume
// =============================================================================

/// Escape a string for safe inclusion in single-quoted shell argument.
/// Handles single quotes by ending the quote, adding escaped quote, reopening.
/// Only used for remote SSH commands where shell invocation is unavoidable.
fn shell_escape(s: &str) -> String {
    s.replace("'", "'\\''")
}

/// Resume or fork a session, handling both local and remote sessions.
fn resume_session(session: &Session, filepath: &std::path::Path, fork: bool) -> Result<()> {
    use std::process::Command;

    if !session.storage.is_live() {
        anyhow::bail!(
            "Cannot resume {} session from {}; restore it first",
            session.agent.display_name(),
            session.storage.display_name()
        );
    }

    let action = if fork { "Forking" } else { "Resuming" };
    let project_path = &session.project_path;

    // Validate project path
    if project_path.is_empty() {
        eprintln!("Error: Session {} has no project path recorded", session.id);
        eprintln!("Session file: {}", filepath.display());
        anyhow::bail!("Cannot resume: no project path");
    }

    let status = match (session.agent, &session.source) {
        (SessionAgent::Codex, SessionSource::Local) => {
            if !std::path::Path::new(project_path).exists() {
                eprintln!(
                    "Error: Project directory no longer exists: {}",
                    project_path
                );
                eprintln!("Session file: {}", filepath.display());
                anyhow::bail!("Cannot resume: directory '{}' not found", project_path);
            }

            println!(
                "{} Codex session {} in {}",
                action, session.id, session.project_path
            );

            let mut cmd = Command::new("codex");
            cmd.current_dir(project_path);
            if fork {
                cmd.args(["fork", &session.id]);
            } else {
                cmd.args(["resume", &session.id]);
            }
            cmd.status()?
        }
        (SessionAgent::Codex, SessionSource::Remote { .. }) => {
            anyhow::bail!("Remote Codex sessions are not supported yet")
        }
        (SessionAgent::Claude, SessionSource::Local) => {
            // Verify directory exists locally
            if !std::path::Path::new(project_path).exists() {
                eprintln!(
                    "Error: Project directory no longer exists: {}",
                    project_path
                );
                eprintln!("Session file: {}", filepath.display());
                anyhow::bail!("Cannot resume: directory '{}' not found", project_path);
            }

            println!(
                "{} session {} in {}",
                action, session.id, session.project_path
            );

            // On Windows, claude is installed as .cmd — use cmd.exe to resolve it.
            // On Unix, invoke directly.
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", "claude"]);
                c
            } else {
                Command::new("claude")
            };
            cmd.current_dir(project_path)
                .args(["-r", &session.id, "--permission-mode", "bypassPermissions"]);
            if fork {
                cmd.arg("--fork-session");
            }
            cmd.status()?
        }
        (SessionAgent::Claude, SessionSource::Remote { name, host, user }) => {
            let ssh_target = match user {
                Some(u) => format!("{}@{}", u, host),
                None => host.clone(),
            };

            println!(
                "{} remote session {} on {} in {}",
                action, session.id, name, session.project_path
            );

            // Remote requires shell string — escape for safe single-quoting
            let fork_flag = if fork { " --fork-session" } else { "" };
            let claude_cmd = format!(
                "cd '{}' && claude -r '{}'{}",
                shell_escape(project_path),
                shell_escape(&session.id),
                fork_flag
            );

            // -t allocates a pseudo-TTY (required for claude's interactive mode)
            Command::new("ssh")
                .args(["-t", &ssh_target, &claude_cmd])
                .status()?
        }
    };

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        eprintln!("Command exited with code {}", code);
        eprintln!("Session file: {}", filepath.display());
    }

    Ok(())
}

// =============================================================================
// Interactive Mode (skim - no external dependencies)
// =============================================================================

/// Build a map of parent session ID → child sessions (forks)
fn build_fork_tree(sessions: &[Session]) -> std::collections::HashMap<&str, Vec<&Session>> {
    use std::collections::HashMap;
    let mut children_map: HashMap<&str, Vec<&Session>> = HashMap::new();

    for session in sessions {
        if let Some(parent_id) = session.forked_from.as_deref() {
            children_map.entry(parent_id).or_default().push(session);
        }
    }

    for children in children_map.values_mut() {
        children.sort_by(|a, b| b.modified.cmp(&a.modified));
    }

    children_map
}

/// Build header showing current navigation state
fn build_subtree_header(
    search_pattern: Option<&str>,
    search_count: Option<usize>,
    fork: bool,
    focus: Option<&str>,
    session_by_id: &std::collections::HashMap<&str, &Session>,
    debug: bool,
) -> String {
    // When searching, show esc to clear; otherwise show navigation hints
    let (nav_hint, focus_info) = if search_pattern.is_some() {
        ("esc to clear", String::new())
    } else {
        let hint = if focus.is_some() {
            "← back │ tab:status │ a:archive │ x:trash │ t:sort"
        } else {
            "→ forks │ tab:status │ a:archive │ x:trash │ t:sort"
        };
        let info = focus
            .and_then(|id| session_by_id.get(id))
            .map(|s| format!(" [{}]", format_session_desc(s, 30)))
            .unwrap_or_default();
        (hint, info)
    };

    let status_line = match (search_pattern, search_count, fork) {
        (Some(pat), Some(count), true) => {
            format!(
                "FORK │ search: \"{}\" ({} matches) │ {}",
                pat, count, nav_hint
            )
        }
        (Some(pat), Some(count), false) => {
            format!("search: \"{}\" ({} matches) │ {}", pat, count, nav_hint)
        }
        (Some(pat), None, true) => format!("FORK │ search: \"{}\" │ {}", pat, nav_hint),
        (Some(pat), None, false) => format!("search: \"{}\" │ {}", pat, nav_hint),
        (None, _, true) => format!("FORK mode │ {}{}", nav_hint, focus_info),
        (None, _, false) => format!("Select session │ {}{}", nav_hint, focus_info),
    };

    let legend = build_column_legend(debug);
    format!("{}\n{}", status_line, legend)
}

/// Width (in columns) consumed by the fixed fields before SUMMARY:
/// status (1) + prefix (2) + CRE (4+1) + MOD (4+1) + MSG (3+1)
/// + AGENT (6+1) + SOURCE (6+1) + PROJECT (12+1).
const FIXED_COLS: usize = 44;

/// Simple session row format (no tree glyphs). `desc_width` is the budget for
/// the trailing summary column — caller computes it from the available pane
/// width so we only truncate when we actually run out of space.
fn format_session_row_simple(
    prefix: &str,
    session: &Session,
    debug: bool,
    desc_width: usize,
) -> String {
    let created = format_time_relative(session.created);
    let modified = format_time_relative(session.modified);
    let agent = session.agent.display_name();
    let source = session.source.display_name();
    let id_prefix = if debug {
        format!("{:<6}", &session.id[..5.min(session.id.len())])
    } else {
        String::new()
    };
    let msgs = format!("{:>3}", session.turn_count);

    // PROJECT column is fixed at 12 chars so FIXED_COLS arithmetic holds.
    // Long project names are middle-elided (keeps both prefix and suffix
    // readable — `claude-cli-internal` → `claud…ternal`).
    let project = elide_middle(&session.project, 12);

    let desc = format_session_desc(session, desc_width);

    format!(
        "{}{}{:<4} {:<4} {} {:<6} {:<6} {:<12} {}",
        prefix, id_prefix, created, modified, msgs, agent, source, project, desc,
    )
}

/// Middle-elide a string to at most `max` chars. Keeps roughly equal head and
/// tail, inserts `…` between them. Returns a `Cow` to avoid allocating when
/// the input already fits.
fn elide_middle(s: &str, max: usize) -> Cow<'_, str> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return Cow::Borrowed(s);
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out = String::with_capacity(max);
    out.extend(&chars[..head]);
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    Cow::Owned(out)
}

/// Available width for the SUMMARY column given the list pane width.
/// Floors at a small minimum so very narrow terminals still show something.
fn desc_budget(pane_width: u16, debug: bool) -> usize {
    let fixed = FIXED_COLS + if debug { 6 } else { 0 };
    (pane_width as usize).saturating_sub(fixed).max(20)
}

/// Build column legend for interactive mode
fn build_column_legend(debug: bool) -> String {
    let id_col = if debug { "ID    " } else { "" };
    format!("  {}CRE  MOD  MSG AGENT  SOURCE PROJECT      SUMMARY", id_col)
}

/// Compute visible sessions based on current search and subtree focus state.
/// Search mode takes priority and temporarily replaces subtree/root views.
fn visible_sessions_for_view<'a>(
    sessions: &'a [Session],
    session_by_id: &std::collections::HashMap<&str, &'a Session>,
    children_map: &std::collections::HashMap<&str, Vec<&'a Session>>,
    search_results: Option<&std::collections::HashSet<String>>,
    focus: Option<&str>,
) -> Vec<&'a Session> {
    if let Some(matched_ids) = search_results {
        return sessions
            .iter()
            .filter(|s| matched_ids.contains(&s.id))
            .collect();
    }

    if let Some(focus_id) = focus {
        let mut result = Vec::new();
        if let Some(session) = session_by_id.get(focus_id) {
            result.push(*session);
            if let Some(children) = children_map.get(focus_id) {
                result.extend(children.iter().copied());
            }
        }
        return result;
    }

    // Root view: only show sessions without a parent (or orphaned forks)
    sessions
        .iter()
        .filter(|s| {
            s.forked_from
                .as_deref()
                .map(|p| !session_by_id.contains_key(p))
                .unwrap_or(true)
        })
        .collect()
}

// =============================================================================
// ANSI → ratatui Text parser
// =============================================================================

/// Parse ANSI-colored text into ratatui styled Text.
/// Handles the specific ANSI codes used by preview functions.
fn parse_ansi_text(input: &str) -> ratatui::text::Text<'static> {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();

    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\x1b' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
            }
            i += 2;
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i < bytes.len() {
                let code = &input[start..i];
                i += 1;
                style = match code {
                    "0" => Style::default(),
                    "1" => style.add_modifier(Modifier::BOLD),
                    "2" => style.add_modifier(Modifier::DIM),
                    "7" => style.add_modifier(Modifier::REVERSED),
                    "1;7" => Style::default()
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    "32" => Style::default().fg(Color::Green),
                    "33" => Style::default().fg(Color::Yellow),
                    "36" => Style::default().fg(Color::Cyan),
                    _ => style,
                };
            }
        } else if bytes[i] == b'\n' {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            i += 1;
        } else {
            let c = input[i..].chars().next().unwrap();
            buf.push(c);
            i += c.len_utf8();
        }
    }

    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::raw("(empty)"));
    }

    ratatui::text::Text::from(lines)
}

// =============================================================================
// Interactive Mode (ratatui + crossterm TUI)
// =============================================================================

fn interactive_mode(mut sessions: Vec<Session>, fork: bool, debug: bool) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
    use ratatui::Terminal;
    use std::collections::HashMap;
    use std::io;

    // Background search index
    let index_targets: Vec<(String, SessionAgent, PathBuf)> = sessions
        .iter()
        .map(|s| (s.id.clone(), s.agent, s.filepath.clone()))
        .collect();
    let mut index_handle = Some(std::thread::spawn(move || build_search_index_for_sessions(index_targets)));
    let mut search_index: Option<claude_code::SearchIndex> = None;

    let mut state = InteractiveState::default();
    let mut filter_text = String::new();
    let mut selected: usize = 0;
    let mut preview_scroll: u16 = 0;
    let mut sort_mode: u8 = 0; // 0=modified, 1=created, 2=turns, 3=project, 4=status

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut selected_session_id: Option<String> = None;
    let mut meta_store = metadata::load();

    'outer: loop {
        if sessions.is_empty() {
            selected_session_id = None;
            break 'outer;
        }

        let session_by_id: HashMap<&str, &Session> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let children_map = build_fork_tree(&sessions);
        let has_children_ids: std::collections::HashSet<String> =
            children_map.keys().map(|id| (*id).to_owned()).collect();

        let focus = state.focus().map(String::as_str);
        let mut visible = visible_sessions_for_view(
            &sessions,
            &session_by_id,
            &children_map,
            state.search_results(),
            focus,
        );

        // Apply sort
        match sort_mode {
            0 => visible.sort_by(|a, b| b.modified.cmp(&a.modified)),
            1 => visible.sort_by(|a, b| b.created.cmp(&a.created)),
            2 => visible.sort_by(|a, b| b.turn_count.cmp(&a.turn_count)),
            3 => visible.sort_by(|a, b| a.project.cmp(&b.project)),
            4 => visible.sort_by(|a, b| {
                let sa = metadata::get_status(&meta_store, &a.id).map(|s| s as u8);
                let sb = metadata::get_status(&meta_store, &b.id).map(|s| s as u8);
                // active(0) first, then paused(1), done(2), then None last
                match (sa, sb) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => b.modified.cmp(&a.modified),
                }
            }),
            _ => {}
        }

        let sort_label = match sort_mode {
            0 => "mod",
            1 => "cre",
            2 => "msg",
            3 => "prj",
            4 => "status",
            _ => "?",
        };

        let term_size = terminal.size()?;
        let desc_width = desc_budget(term_size.width / 2, debug);

        // Build display rows: (row_text, session_id, is_named)
        let display_items: Vec<(String, String, bool)> = visible
            .iter()
            .map(|session| {
                let prefix = if focus == Some(session.id.as_str()) {
                    "▷ "
                } else if children_map.contains_key(session.id.as_str()) {
                    "▶ "
                } else {
                    "  "
                };
                let st = metadata::get_status(&meta_store, &session.id)
                    .map(|s| s.indicator())
                    .unwrap_or(" ");
                (
                    format!("{}{}", st, format_session_row_simple(prefix, session, debug, desc_width)),
                    session.id.clone(),
                    session.name.is_some(),
                )
            })
            .collect();

        // Apply text filter
        let filtered: Vec<usize> = if filter_text.is_empty() {
            (0..display_items.len()).collect()
        } else {
            let f = filter_text.to_lowercase();
            display_items
                .iter()
                .enumerate()
                .filter(|(_, (row, _, _))| row.to_lowercase().contains(&f))
                .map(|(i, _)| i)
                .collect()
        };

        // Clamp selection
        if filtered.is_empty() {
            selected = 0;
        } else if selected >= filtered.len() {
            selected = filtered.len() - 1;
        }

        // Generate preview for selected session
        let preview_content = filtered
            .get(selected)
            .and_then(|&idx| sessions.iter().find(|s| s.id == display_items[idx].1))
            .and_then(|session| {
                match state.search_pattern() {
                    Some(pat) => generate_search_preview_for_session(session, pat),
                    None => generate_preview_for_session(session),
                }
                .ok()
            })
            .unwrap_or_default();

        // Build header
        let header_text = build_subtree_header(
            state.search_pattern().map(String::as_str),
            state.search_results().map(|r| r.len()),
            fork,
            focus,
            &session_by_id,
            debug,
        );

        // Render
        let mut list_state = ListState::default();
        if !filtered.is_empty() {
            list_state.select(Some(selected));
        }

        terminal.draw(|f| {
            let area = f.area();

            let h_split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);

            // Left: header + filter + list
            let v_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(1)])
                .split(h_split[0]);

            // Header + filter input
            let mut header_lines: Vec<Line> = header_text
                .lines()
                .map(|l| Line::styled(l.to_string(), Style::default().fg(Color::DarkGray)))
                .collect();
            header_lines.push(Line::from(vec![
                Span::styled(
                    format!("[{}] filter> ", sort_label),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(filter_text.clone()),
                Span::styled("█", Style::default().fg(Color::White)),
            ]));
            f.render_widget(Paragraph::new(header_lines), v_split[0]);

            // Session list
            let items: Vec<ListItem> = filtered
                .iter()
                .map(|&idx| {
                    let (row, _, named) = &display_items[idx];
                    let style = if *named {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(row.as_str()).style(style)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ");
            f.render_stateful_widget(list, v_split[1], &mut list_state);

            // Right: preview
            let preview_text = parse_ansi_text(&preview_content);
            let preview = Paragraph::new(preview_text)
                .block(Block::default().borders(Borders::LEFT).title(" Preview "))
                .wrap(Wrap { trim: false })
                .scroll((preview_scroll, 0));
            f.render_widget(preview, h_split[1]);
        })?;

        drop(visible);
        drop(children_map);
        drop(session_by_id);

        // Handle input
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Windows emits both Press and Release events — only handle Press
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => match state.apply(StateAction::Esc) {
                        StateEffect::Exit => break 'outer,
                        _ => {
                            filter_text.clear();
                            selected = 0;
                            preview_scroll = 0;
                        }
                    },
                    (KeyCode::Enter, _) => {
                        if let Some(&idx) = filtered.get(selected) {
                            let id = display_items[idx].1.clone();
                            if let StateEffect::Select { session_id } =
                                state.apply(StateAction::Enter {
                                    selected_id: Some(id),
                                })
                            {
                                selected_session_id = Some(session_id);
                                break 'outer;
                            }
                        }
                    }
                    (KeyCode::Up, _) => {
                        if selected > 0 {
                            selected -= 1;
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Down, _) => {
                        if selected + 1 < filtered.len() {
                            selected += 1;
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Right, _) => {
                        if let Some(&idx) = filtered.get(selected) {
                            let id = display_items[idx].1.clone();
                            let has_children = has_children_ids.contains(id.as_str());
                            let _ = state.apply(StateAction::Right {
                                selected_id: Some(id),
                                has_children,
                            });
                            selected = 0;
                            filter_text.clear();
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Left, _) => {
                        let _ = state.apply(StateAction::Left);
                        selected = 0;
                        filter_text.clear();
                        preview_scroll = 0;
                    }
                    (KeyCode::Char('s'), m) if m.contains(KeyModifiers::CONTROL) => {
                        let effect = state.apply(StateAction::CtrlS {
                            query: filter_text.clone(),
                        });
                        if let StateEffect::RunSearch { pattern } = effect {
                            let index = search_index.get_or_insert_with(|| {
                                index_handle
                                    .take()
                                    .and_then(|h| h.join().ok())
                                    .unwrap_or_default()
                            });
                            let pattern_lower = pattern.to_ascii_lowercase();
                            let matched_ids: std::collections::HashSet<String> = index
                                .iter()
                                .filter(|(_, text)| text.contains(&pattern_lower))
                                .map(|(id, _)| id.clone())
                                .collect();
                            let _ = state.apply(StateAction::ApplySearchResults {
                                pattern,
                                matched_ids,
                            });
                            filter_text.clear();
                            selected = 0;
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        break 'outer;
                    }
                    (KeyCode::Tab, _) => {
                        // Cycle status: none → active → paused → done → none
                        if let Some(&idx) = filtered.get(selected) {
                            let id = display_items[idx].1.as_str();
                            metadata::cycle_status(&mut meta_store, id);
                            let _ = metadata::save(&meta_store);
                        }
                    }
                    (KeyCode::Char('d'), m)
                        if !m.contains(KeyModifiers::CONTROL) && filter_text.is_empty() =>
                    {
                        if let Some(&idx) = filtered.get(selected) {
                            let id = display_items[idx].1.as_str();
                            metadata::set_status(&mut meta_store, id, Some(metadata::Status::Done));
                            let _ = metadata::save(&meta_store);
                            sessions.retain(|s| s.id != id);
                            state = InteractiveState::default();
                            selected = selected.saturating_sub(1);
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Char('a'), m)
                        if !m.contains(KeyModifiers::CONTROL) && filter_text.is_empty() =>
                    {
                        let selected_session = filtered
                            .get(selected)
                            .and_then(|&idx| sessions.iter().find(|s| s.id == display_items[idx].1))
                            .cloned();
                        if let Some(session) = selected_session
                            && session.storage.is_live()
                            && archive::archive_session(&session).is_ok()
                        {
                            sessions.retain(|s| s.id != session.id);
                            state = InteractiveState::default();
                            selected = selected.saturating_sub(1);
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Char('x'), m)
                        if !m.contains(KeyModifiers::CONTROL) && filter_text.is_empty() =>
                    {
                        let selected_session = filtered
                            .get(selected)
                            .and_then(|&idx| sessions.iter().find(|s| s.id == display_items[idx].1))
                            .cloned();
                        if let Some(session) = selected_session
                            && session.storage.is_live()
                            && archive::trash_session(&session).is_ok()
                        {
                            sessions.retain(|s| s.id != session.id);
                            state = InteractiveState::default();
                            selected = selected.saturating_sub(1);
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Char('r'), m)
                        if !m.contains(KeyModifiers::CONTROL) && filter_text.is_empty() =>
                    {
                        let selected_session = filtered
                            .get(selected)
                            .and_then(|&idx| sessions.iter().find(|s| s.id == display_items[idx].1))
                            .cloned();
                        if let Some(session) = selected_session
                            && !session.storage.is_live()
                            && archive::restore_session(&session).is_ok()
                        {
                            sessions.retain(|s| s.id != session.id);
                            state = InteractiveState::default();
                            selected = selected.saturating_sub(1);
                            preview_scroll = 0;
                        }
                    }
                    (KeyCode::Char('t'), m) if !m.contains(KeyModifiers::CONTROL) && filter_text.is_empty() => {
                        sort_mode = (sort_mode + 1) % 5;
                        selected = 0;
                        preview_scroll = 0;
                    }
                    (KeyCode::PageDown, _) => {
                        preview_scroll = preview_scroll.saturating_add(10);
                    }
                    (KeyCode::PageUp, _) => {
                        preview_scroll = preview_scroll.saturating_sub(10);
                    }
                    (KeyCode::Char(c), _) => {
                        filter_text.push(c);
                        selected = 0;
                        preview_scroll = 0;
                    }
                    (KeyCode::Backspace, _) => {
                        filter_text.pop();
                        selected = 0;
                        preview_scroll = 0;
                    }
                    _ => {}
                }
            }
        }
    }

    // Restore terminal
    terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    let _ = std::panic::take_hook();

    // Resume selected session
    if let Some(session_id) = selected_session_id
        && let Some(session) = sessions.iter().find(|s| s.id == session_id)
    {
        resume_session(session, &session.filepath, fork)?;
    }

    Ok(())
}

// =============================================================================
// Tests (general functionality)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Project filter logic - The -p flag behavior
    // =========================================================================

    #[test]
    fn project_filter_case_insensitive() {
        let projects = [
            "holy-grail",
            "Ministry-Of-Silly-Walks",
            "SPANISH-INQUISITION",
        ];

        let matches = |filter: &str| -> Vec<&str> {
            let filter_lower = filter.to_lowercase();
            projects
                .iter()
                .filter(|p| p.to_lowercase().contains(&filter_lower))
                .copied()
                .collect()
        };

        assert_eq!(matches("spanish"), ["SPANISH-INQUISITION"]);
        assert_eq!(matches("SILLY"), ["Ministry-Of-Silly-Walks"]);
        assert_eq!(matches("grail"), ["holy-grail"]);
    }

    #[test]
    fn project_filter_substring() {
        let projects = ["spam", "spam-eggs", "spam-eggs-spam"];

        let matches = |filter: &str| -> Vec<&str> {
            let filter_lower = filter.to_lowercase();
            projects
                .iter()
                .filter(|p| p.to_lowercase().contains(&filter_lower))
                .copied()
                .collect()
        };

        assert_eq!(matches("spam"), ["spam", "spam-eggs", "spam-eggs-spam"]);
        assert_eq!(matches("eggs"), ["spam-eggs", "spam-eggs-spam"]);
    }

    // =========================================================================
    // Text normalization
    // =========================================================================

    #[test]
    fn normalize_summary_collapses_whitespace() {
        assert_eq!(
            normalize_summary("hello   world\n\ntest", 50),
            "hello world test"
        );
    }

    #[test]
    fn normalize_summary_strips_markdown() {
        assert_eq!(normalize_summary("# Heading", 50), "Heading");
        assert_eq!(normalize_summary("## Sub heading", 50), "Sub heading");
        assert_eq!(normalize_summary("* bullet point", 50), "bullet point");
    }

    #[test]
    fn normalize_summary_truncates_at_word() {
        // Should truncate at word boundary when possible
        let result = normalize_summary("hello world this is a test", 15);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 18); // 15 + "..."
    }

    #[test]
    fn normalize_summary_preserves_short_text() {
        assert_eq!(normalize_summary("short", 50), "short");
    }

    // =========================================================================
    // Time formatting
    // =========================================================================

    #[test]
    fn format_time_relative_now() {
        let now = SystemTime::now();
        assert_eq!(format_time_relative(now), "now");
    }

    #[test]
    fn format_time_relative_minutes() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(120);
        assert_eq!(format_time_relative(time), "2m");
    }

    #[test]
    fn format_time_relative_hours() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(3600 * 3);
        assert_eq!(format_time_relative(time), "3h");
    }

    #[test]
    fn format_time_relative_days() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(86400 * 2);
        assert_eq!(format_time_relative(time), "2d");
    }

    #[test]
    fn format_time_relative_weeks() {
        use std::time::Duration;
        let time = SystemTime::now() - Duration::from_secs(604800 * 3);
        assert_eq!(format_time_relative(time), "3w");
    }

    #[test]
    fn format_time_relative_future() {
        use std::time::Duration;
        let time = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(format_time_relative(time), "?");
    }

    // =========================================================================
    // Fork list and tree view
    // =========================================================================

    fn test_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            agent: SessionAgent::Claude,
            project: "test-project".to_string(),
            project_path: "/tmp/test-project".to_string(),
            filepath: PathBuf::from(format!("/tmp/{}.jsonl", id)),
            created: SystemTime::now(),
            modified: SystemTime::now(),
            first_message: None,
            summary: Some("test summary".to_string()),
            name: None,
            tag: None,
            turn_count: 1,
            source: SessionSource::Local,
            storage: SessionStorage::Live,
            forked_from: None,
        }
    }

    #[test]
    fn list_mode_excludes_forks_by_default() {
        let parent = test_session("parent");
        let mut fork = test_session("fork");
        fork.forked_from = Some("parent".to_string());

        let sessions = vec![parent, fork];
        let visible = filter_forks_for_list(&sessions, false);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "parent");
    }

    // =========================================================================
    // Fork tree and subtree collection
    // =========================================================================

    #[test]
    fn build_fork_tree_maps_parent_to_children() {
        let root = test_session("root");
        let mut child1 = test_session("child1");
        child1.forked_from = Some("root".to_string());
        let mut child2 = test_session("child2");
        child2.forked_from = Some("root".to_string());

        let sessions = vec![root, child1, child2];
        let children_map = build_fork_tree(&sessions);

        assert!(children_map.contains_key("root"));
        assert_eq!(children_map.get("root").unwrap().len(), 2);
        assert!(!children_map.contains_key("child1"));
        assert!(!children_map.contains_key("child2"));
    }

    #[test]
    fn build_fork_tree_handles_nested_forks() {
        // root -> child -> grandchild
        let root = test_session("root");
        let mut child = test_session("child");
        child.forked_from = Some("root".to_string());
        let mut grandchild = test_session("grandchild");
        grandchild.forked_from = Some("child".to_string());

        let sessions = vec![root, child, grandchild];
        let children_map = build_fork_tree(&sessions);

        assert_eq!(children_map.get("root").unwrap().len(), 1);
        assert_eq!(children_map.get("child").unwrap().len(), 1);
        assert!(!children_map.contains_key("grandchild"));
    }

    // =========================================================================
    // Column legend and header formatting
    // =========================================================================

    #[test]
    fn build_column_legend_without_debug() {
        let legend = build_column_legend(false);
        assert_eq!(legend, "  CRE  MOD  MSG AGENT  SOURCE PROJECT      SUMMARY");
        assert!(!legend.contains("ID"));
    }

    #[test]
    fn build_column_legend_with_debug() {
        let legend = build_column_legend(true);
        assert!(legend.contains("ID"));
        assert!(legend.contains("CRE"));
        assert!(legend.contains("MSG"));
    }

    #[test]
    fn build_subtree_header_root_view() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(None, None, false, None, &session_by_id, false);
        assert!(header.contains("Select session"));
        assert!(header.contains("→ forks"));
        assert!(header.contains("CRE")); // Legend line
    }

    #[test]
    fn build_subtree_header_fork_mode() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(None, None, true, None, &session_by_id, false);
        assert!(header.contains("FORK mode"));
    }

    #[test]
    fn build_subtree_header_with_search() {
        use std::collections::HashMap;
        let session_by_id: HashMap<&str, &Session> = HashMap::new();

        let header = build_subtree_header(Some("api"), Some(5), false, None, &session_by_id, false);
        assert!(header.contains("search: \"api\""));
        assert!(header.contains("(5 matches)"));
        assert!(header.contains("esc to clear"));
    }

    #[test]
    fn build_subtree_header_focused_shows_back() {
        use std::collections::HashMap;
        let session = test_session("focused");
        let mut session_by_id: HashMap<&str, &Session> = HashMap::new();
        session_by_id.insert("focused", &session);

        let header =
            build_subtree_header(None, None, false, Some("focused"), &session_by_id, false);
        assert!(header.contains("← back"));
        assert!(!header.contains("→ forks"));
    }

    // =========================================================================
    // Session row formatting
    // =========================================================================

    #[test]
    fn format_session_row_simple_basic() {
        let session = test_session("test-id");
        let row = format_session_row_simple("  ", &session, false, 40);

        // Should contain project name and source
        assert!(row.contains("test-proj"));
        assert!(row.contains("local"));
        // Should NOT start with ID prefix when debug=false (starts with "  " prefix)
        assert!(row.starts_with("  "));
        // ID "test-id" first 5 chars is "test-" which should NOT appear at start
        assert!(!row.starts_with("  test-"));
    }

    #[test]
    fn format_session_row_simple_with_debug() {
        let session = test_session("abcdef-1234");
        let row = format_session_row_simple("▶ ", &session, true, 40);

        // Should contain first 5 chars of ID
        assert!(row.contains("abcde"));
        // Should contain the prefix
        assert!(row.starts_with("▶ "));
    }

    #[test]
    fn elide_middle_passthrough_when_fits() {
        assert_eq!(elide_middle("short", 12), "short");
        assert_eq!(elide_middle("exactly-12ch", 12), "exactly-12ch");
    }

    #[test]
    fn elide_middle_shortens_long_names() {
        let out = elide_middle("claude-cli-internal", 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.contains('…'));
        // Keeps head and tail readable
        assert!(out.starts_with("claud"));
        assert!(out.ends_with("ternal"));
    }

    #[test]
    fn desc_budget_scales_with_pane_width() {
        // 200-col pane -> 200 - 44 fixed = 156
        assert_eq!(desc_budget(200, false), 156);
        // Debug adds 6 for the ID prefix
        assert_eq!(desc_budget(200, true), 150);
        // Narrow pane floors at 20
        assert_eq!(desc_budget(40, false), 20);
    }

    #[test]
    fn format_session_row_simple_shows_turn_count() {
        let mut session = test_session("test");
        session.turn_count = 42;
        let row = format_session_row_simple("  ", &session, false, 40);

        // Turn count should be right-aligned in 3 chars
        assert!(row.contains(" 42 "));
    }

    // =========================================================================
    // Shell escaping (security)
    // =========================================================================

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("/path/to/project"), "/path/to/project");
    }

    #[test]
    fn shell_escape_single_quotes() {
        // Single quote becomes: end quote, escaped quote, start quote
        assert_eq!(shell_escape("it's"), "it'\\''s");
        assert_eq!(shell_escape("'quoted'"), "'\\''quoted'\\''");
    }

    #[test]
    fn shell_escape_multiple_quotes() {
        assert_eq!(shell_escape("a'b'c"), "a'\\''b'\\''c");
    }

    #[test]
    fn shell_escape_preserves_other_chars() {
        // Double quotes, spaces, etc. are fine inside single quotes
        assert_eq!(shell_escape("hello world"), "hello world");
        assert_eq!(shell_escape("\"quoted\""), "\"quoted\"");
        assert_eq!(shell_escape("$HOME"), "$HOME");
    }

    // =========================================================================
    // Highlight matching (Unicode-safe)
    // =========================================================================

    #[test]
    fn highlight_match_basic() {
        let result = highlight_match("hello world", "world");
        assert!(result.contains(colors::BOLD_INVERSE));
        assert!(result.contains("world"));
        assert!(result.contains(colors::RESET));
    }

    #[test]
    fn highlight_match_case_insensitive() {
        let result = highlight_match("Hello World", "world");
        // Should highlight "World" (preserving original case)
        assert!(result.contains("World"));
        assert!(result.contains(colors::BOLD_INVERSE));
    }

    #[test]
    fn highlight_match_empty_pattern() {
        assert_eq!(highlight_match("hello", ""), "hello");
    }

    #[test]
    fn highlight_match_no_match() {
        let result = highlight_match("hello", "xyz");
        assert!(!result.contains(colors::BOLD_INVERSE));
        assert_eq!(result, "hello");
    }

    #[test]
    fn highlight_match_multibyte_chars() {
        // Test with emoji and Unicode - should not panic
        let result = highlight_match("hello 🌍 world", "world");
        assert!(result.contains(colors::BOLD_INVERSE));
    }

    #[test]
    fn highlight_match_unicode_case_fold() {
        // ß lowercases to "ss" - pattern "ss" should still work
        // The text has ß, searching for "ss" should not find it (different chars)
        // But searching for "ß" in text with "ß" should work
        let result = highlight_match("Straße", "ße");
        assert!(result.contains(colors::BOLD_INVERSE));
    }

    #[test]
    fn search_results_replace_subtree_until_esc() {
        use std::collections::{HashMap, HashSet};

        let root = test_session("root");
        let mut child = test_session("child");
        child.forked_from = Some("root".to_string());
        let sibling = test_session("sibling");

        let sessions = vec![root, child, sibling];
        let session_by_id: HashMap<&str, &Session> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let children_map = build_fork_tree(&sessions);

        // Focused subtree should show root + child
        let visible =
            visible_sessions_for_view(&sessions, &session_by_id, &children_map, None, Some("root"));
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "child"]);

        // Search should replace subtree view
        let mut matched = HashSet::new();
        matched.insert("sibling".to_string());
        let visible = visible_sessions_for_view(
            &sessions,
            &session_by_id,
            &children_map,
            Some(&matched),
            Some("root"),
        );
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["sibling"]);

        // Clearing search restores subtree view
        let visible =
            visible_sessions_for_view(&sessions, &session_by_id, &children_map, None, Some("root"));
        let ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "child"]);
    }

    #[test]
    fn strict_mode_fails_when_any_remote_sync_fails() {
        let result = enforce_strict_mode(true, 1, 0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Strict mode: 1 sync source(s) failed")
        );
    }

    #[test]
    fn strict_mode_fails_when_any_discovery_source_fails() {
        let result = enforce_strict_mode(true, 0, 2);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Strict mode: 2 discovery source(s) failed")
        );
    }

    #[test]
    fn strict_mode_disabled_allows_failures() {
        assert!(enforce_strict_mode(false, 3, 4).is_ok());
    }

    #[test]
    fn session_source_display_name_local() {
        let source = crate::session::SessionSource::Local;
        assert_eq!(source.display_name(), "local");
        assert!(source.is_local());
    }
}

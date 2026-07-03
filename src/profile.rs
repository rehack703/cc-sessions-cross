//! cc-multiplexor profile launcher.
//!
//! Each ccm profile is a `CLAUDE_CONFIG_DIR` under CCM_HOME = a separate Claude
//! account. We ask each account for its remaining limits via `claude -p "/usage"`
//! — Claude Code answers using its own stored credentials, so we never read or
//! handle the token ourselves — then pick the account with the most headroom and
//! launch it. Registration shells out to `claude auth login` in a fresh dir.

use crate::claude_code::{find_profiles, get_ccm_home};
use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A ccm profile: a named `CLAUDE_CONFIG_DIR`.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub config_dir: PathBuf,
}

/// Parsed `/usage` output for one account.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// 5-hour rolling window, percent used.
    pub session_pct: Option<u8>,
    /// Weekly (all models) window, percent used.
    pub week_pct: Option<u8>,
    pub session_reset: Option<String>,
    pub week_reset: Option<String>,
}

impl Usage {
    /// The tightest limit's used%. Lower = more headroom; missing data sorts worst.
    pub fn pressure(&self) -> u8 {
        self.session_pct
            .unwrap_or(100)
            .max(self.week_pct.unwrap_or(100))
    }
}

/// Parse the text emitted by `claude -p "/usage"`. Lines are order-independent:
///
/// ```text
/// Current session: 22% used · resets Jun 11, 1:30pm (Asia/Seoul)
/// Current week (all models): 76% used · resets Jun 13, 2am (Asia/Seoul)
/// ```
pub fn parse_usage(out: &str) -> Usage {
    let mut u = Usage::default();
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Current session:") {
            (u.session_pct, u.session_reset) = parse_pct_reset(rest);
        } else if let Some(rest) = line.strip_prefix("Current week (all models):") {
            (u.week_pct, u.week_reset) = parse_pct_reset(rest);
        }
    }
    u
}

/// From " 22% used · resets Jun 11, 1:30pm" → (Some(22), Some("Jun 11, 1:30pm")).
fn parse_pct_reset(rest: &str) -> (Option<u8>, Option<String>) {
    let rest = rest.trim();
    let pct = rest
        .split('%')
        .next()
        .and_then(|s| s.trim().parse::<u8>().ok());
    let reset = rest
        .split("resets")
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (pct, reset)
}

/// Accounts to compare. With no custom profiles there is nothing to choose
/// between, so this stays empty (no TUI header; `--usage` prints the hint).
/// Once at least one profile exists, the default `~/.claude` account is included
/// as `local` so auto-select also weighs the currently-running account.
pub fn profiles() -> Vec<Profile> {
    let custom: Vec<Profile> = find_profiles()
        .into_iter()
        .map(|(name, config_dir)| Profile { name, config_dir })
        .collect();
    if custom.is_empty() {
        return Vec::new();
    }
    let mut all = Vec::with_capacity(custom.len() + 1);
    if let Some(dir) = default_config_dir() {
        all.push(Profile {
            name: "local".into(),
            config_dir: dir,
        });
    }
    all.extend(custom);
    all
}

/// The account Claude Code uses by default: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn default_config_dir() -> Option<PathBuf> {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => dirs::home_dir().map(|h| h.join(".claude")),
    }
}

/// Ask one profile's account for its current usage.
pub fn query_usage(config_dir: &Path) -> Result<Usage> {
    let out = Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .args(["-p", "/usage"])
        .stdin(Stdio::null())
        .output()
        .context("failed to run `claude -p /usage`")?;
    if !out.status.success() {
        bail!(
            "claude exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_usage(&String::from_utf8_lossy(&out.stdout)))
}

/// Query every profile's usage in parallel.
pub fn query_all() -> Vec<(Profile, Result<Usage>)> {
    profiles()
        .into_par_iter()
        .map(|p| {
            let u = query_usage(&p.config_dir);
            (p, u)
        })
        .collect()
}

/// Index of the most-headroom profile (lowest pressure). None if no usable data.
fn best_index(results: &[(Profile, Result<Usage>)]) -> Option<usize> {
    results
        .iter()
        .enumerate()
        .filter_map(|(i, (_, u))| u.as_ref().ok().map(|u| (i, u.pressure())))
        .min_by_key(|&(_, p)| p)
        .map(|(i, _)| i)
}

fn fmt_pct(p: Option<u8>) -> String {
    p.map(|v| format!("{v}%")).unwrap_or_else(|| "-".into())
}

/// `--usage`: print each profile's remaining limits.
pub fn cmd_usage() -> Result<()> {
    let results = query_all();
    if results.is_empty() {
        println!("No profiles found under {}.", ccm_home_display());
        println!("Register one with: cc-sessions --login <name>");
        return Ok(());
    }
    println!(
        "{:<16} {:>8} {:>6}  {}",
        "PROFILE", "SESSION", "WEEK", "WEEK RESETS"
    );
    for (p, u) in &results {
        match u {
            Ok(u) => println!(
                "{:<16} {:>8} {:>6}  {}",
                p.name,
                fmt_pct(u.session_pct),
                fmt_pct(u.week_pct),
                u.week_reset.as_deref().unwrap_or("-"),
            ),
            Err(e) => println!("{:<16} {:>8} {:>6}  (error: {e})", p.name, "?", "?"),
        }
    }
    Ok(())
}

/// TUI header summary from fetched results: `work 27/77  personal 30/50★`.
/// The most-headroom profile is marked with ★.
pub fn summary_line(results: &[(Profile, Result<Usage>)]) -> String {
    let best = best_index(results);
    results
        .iter()
        .enumerate()
        .map(|(i, (p, u))| {
            let star = if Some(i) == best { "★" } else { "" };
            match u {
                Ok(u) => format!(
                    "{} {}/{}{}",
                    p.name,
                    fmt_pct(u.session_pct),
                    fmt_pct(u.week_pct),
                    star
                ),
                Err(_) => format!("{} ?/?", p.name),
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// `--auto`: query all profiles, then launch the most-headroom one.
pub fn cmd_auto() -> Result<()> {
    launch_best(&query_all())
}

/// Pick the most-headroom profile from pre-fetched results and start a new
/// session there. Used by both `--auto` and the TUI (which already has results).
pub fn launch_best(results: &[(Profile, Result<Usage>)]) -> Result<()> {
    if results.is_empty() {
        bail!("No profiles found. Register one with: cc-sessions --login <name>");
    }
    let idx = best_index(results).ok_or_else(|| anyhow!("could not read usage from any profile"))?;
    let (profile, usage) = &results[idx];
    let u = usage.as_ref().expect("best_index returns only Ok entries");
    eprintln!(
        "→ {} (session {}, week {})",
        profile.name,
        fmt_pct(u.session_pct),
        fmt_pct(u.week_pct),
    );
    launch(&profile.config_dir)
}

/// Run `claude` under a profile's config dir (new session), inheriting the
/// terminal. Mirrors resume_session: bypass permission prompts.
fn launch(config_dir: &Path) -> Result<()> {
    let status = Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .args(["--permission-mode", "bypassPermissions"])
        .status()
        .context("failed to launch claude")?;
    if !status.success() {
        bail!("claude exited with {status}");
    }
    Ok(())
}

/// `--login <name>`: create the profile dir and run `claude auth login` in it.
pub fn cmd_login(name: &str) -> Result<()> {
    if !is_valid_profile_name(name) {
        bail!("invalid profile name '{name}' (use letters, digits, ., _, -)");
    }
    let home = get_ccm_home().ok_or_else(|| anyhow!("could not resolve CCM_HOME"))?;
    let dir = home.join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let status = Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", &dir)
        .args(["auth", "login"])
        .status()
        .context("failed to run `claude auth login`")?;
    if !status.success() {
        bail!("`claude auth login` exited with {status}");
    }
    println!("Profile '{name}' ready at {}", dir.display());
    Ok(())
}

/// Same restriction as ccm: letters, digits, `.`, `_`, `-`.
fn is_valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn ccm_home_display() -> String {
    get_ccm_home()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.claude-profiles".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "You are currently using your subscription to power your Claude Code usage\n\nCurrent session: 22% used · resets Jun 11, 1:30pm (Asia/Seoul)\nCurrent week (all models): 76% used · resets Jun 13, 2am (Asia/Seoul)\nCurrent week (Sonnet only): 2% used · resets Jun 13, 2am (Asia/Seoul)\n";

    #[test]
    fn parse_usage_extracts_pct_and_reset() {
        let u = parse_usage(SAMPLE);
        assert_eq!(u.session_pct, Some(22));
        assert_eq!(u.week_pct, Some(76));
        assert_eq!(u.session_reset.as_deref(), Some("Jun 11, 1:30pm (Asia/Seoul)"));
        assert_eq!(u.week_reset.as_deref(), Some("Jun 13, 2am (Asia/Seoul)"));
        assert_eq!(u.pressure(), 76);
    }

    #[test]
    fn parse_usage_handles_missing() {
        let u = parse_usage("nothing relevant here");
        assert_eq!(u.session_pct, None);
        assert_eq!(u.week_pct, None);
        assert_eq!(u.pressure(), 100);
    }

    #[test]
    fn best_index_picks_lowest_pressure() {
        let mk = |n: &str, s: u8, w: u8| -> (Profile, Result<Usage>) {
            (
                Profile {
                    name: n.into(),
                    config_dir: PathBuf::from("/x"),
                },
                Ok(Usage {
                    session_pct: Some(s),
                    week_pct: Some(w),
                    ..Default::default()
                }),
            )
        };
        // pressures: a=max(50,76)=76, b=max(22,30)=30, c=max(10,90)=90 → best=b
        let results = vec![mk("a", 50, 76), mk("b", 22, 30), mk("c", 10, 90)];
        assert_eq!(best_index(&results), Some(1));
    }

    #[test]
    fn best_index_skips_errors() {
        let results = vec![
            (
                Profile {
                    name: "err".into(),
                    config_dir: PathBuf::from("/x"),
                },
                Err(anyhow!("logged out")),
            ),
            (
                Profile {
                    name: "ok".into(),
                    config_dir: PathBuf::from("/y"),
                },
                Ok(Usage {
                    session_pct: Some(5),
                    week_pct: Some(5),
                    ..Default::default()
                }),
            ),
        ];
        assert_eq!(best_index(&results), Some(1));
    }

    #[test]
    fn summary_line_marks_most_headroom() {
        let mk = |n: &str, s: u8, w: u8| -> (Profile, Result<Usage>) {
            (
                Profile {
                    name: n.into(),
                    config_dir: PathBuf::from("/x"),
                },
                Ok(Usage {
                    session_pct: Some(s),
                    week_pct: Some(w),
                    ..Default::default()
                }),
            )
        };
        // personal pressure=30 < work pressure=77 → personal starred.
        let line = summary_line(&[mk("work", 27, 77), mk("personal", 10, 30)]);
        assert!(line.contains("personal 10%/30%★"), "{line}");
        assert!(line.contains("work 27%/77%"), "{line}");
        assert!(!line.contains("work 27%/77%★"), "{line}");
    }

    #[test]
    fn valid_names() {
        assert!(is_valid_profile_name("work"));
        assert!(is_valid_profile_name("acc-1.2_x"));
        assert!(!is_valid_profile_name(""));
        assert!(!is_valid_profile_name("bad name"));
        assert!(!is_valid_profile_name("a/b"));
    }
}

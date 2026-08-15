//! Where readout reads from, and the one place it writes to.
//!
//! Read scope is deliberately narrow: only the transcript directories.
//! `~/.claude/settings.json`, `~/.codex/config.toml` and `~/.codex/auth.json`
//! hold live API credentials and are never opened — not read, not copied,
//! not backed up. Nothing in this crate needs them.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `~/.claude/projects` — one subdirectory per project, each holding session
/// transcripts plus nested subagent/workflow transcripts.
pub fn claude_projects_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("READOUT_CLAUDE_DIR") {
        return Some(PathBuf::from(v));
    }
    let home = dirs::home_dir()?;
    let dir = home.join(".claude").join("projects");
    dir.is_dir().then_some(dir)
}

/// `~/.codex/sessions` — rollout transcripts bucketed as `YYYY/MM/DD/*.jsonl`.
pub fn codex_sessions_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("READOUT_CODEX_DIR") {
        return Some(PathBuf::from(v));
    }
    let home = dirs::home_dir()?;
    let dir = home.join(".codex").join("sessions");
    dir.is_dir().then_some(dir)
}

/// Our own state directory. Never inside `~/.claude` or `~/.codex`.
pub fn state_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("READOUT_STATE_DIR") {
        let p = PathBuf::from(v);
        std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
        return Ok(p);
    }
    let base = dirs::cache_dir()
        .or_else(dirs::home_dir)
        .context("cannot locate a cache or home directory")?;
    let dir = base.join("readout");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

pub fn cache_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("scan-cache.json"))
}

/// User-editable price overrides. Absent by default.
pub fn pricing_override_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("pricing.json"))
}

/// Fallback label for a Claude project directory.
///
/// Claude flattens the working directory into a single name by replacing both
/// path separators and dots with dashes, which is lossy — `readout-cli` and
/// `readout/cli` encode identically. Rather than guess a split, we only trim
/// the leading separator. In practice this rarely shows: every real transcript
/// record carries a `cwd` field, so [`project_label_from_cwd`] wins.
pub fn project_label_from_claude_dir(dir_name: &str) -> String {
    let trimmed = dir_name.trim_start_matches('-');
    if trimmed.is_empty() { dir_name.to_string() } else { trimmed.to_string() }
}

/// Preserve the full working directory as the project identity.
///
/// Basenames are pleasant to read but not unique: `/work/client/api` and
/// `/home/me/api` must not collapse into one project and one CLI filter. The
/// renderers already ellipsize long labels where necessary.
pub fn project_label_from_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim();
    let without_trailing = trimmed.trim_end_matches(['/', '\\']);
    if without_trailing.is_empty() || without_trailing.ends_with(':') {
        trimmed.to_string()
    } else {
        without_trailing.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_dir_names_only_lose_the_leading_separator() {
        assert_eq!(
            project_label_from_claude_dir("-home-u-proj-readout-cli"),
            "home-u-proj-readout-cli"
        );
        assert_eq!(project_label_from_claude_dir("-home-u"), "home-u");
        assert_eq!(project_label_from_claude_dir(""), "");
    }

    #[test]
    fn cwd_remains_an_unambiguous_project_identity() {
        assert_eq!(project_label_from_cwd("/home/u/proj/readout-cli"), "/home/u/proj/readout-cli");
        assert_eq!(project_label_from_cwd("/home/u/proj/"), "/home/u/proj");
        assert_eq!(project_label_from_cwd(r"C:\work\api"), r"C:\work\api");
        assert_eq!(project_label_from_cwd("/"), "/");
    }
}

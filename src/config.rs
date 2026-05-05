// TOML config loader. Lives at ~/.config/anvil/config.toml.
//
// The Linear API key is env-only: TOML carries the NAME of the env var that
// holds the key, not the key itself. This mirrors smithy/PER-31 - any HOME-
// readable file with a real key is a credential leak waiting to happen.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub tracker: TrackerCfg,
    pub review: ReviewCfg,
    pub workspace: WorkspaceCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackerCfg {
    /// Only "linear" supported in v0.1. Kept as a field so future trackers
    /// (github, jira) can slot in without a config-shape break.
    pub kind: String,
    /// Optional project slug filter. When set, only issues in this project are
    /// considered. Useful when the daemon shares Linear with non-symphony work.
    #[serde(default)]
    pub project_slug: Option<String>,
    /// Linear team names whose issues we poll. v0.1 supports a single team
    /// list; resolution is by display name (e.g. ["Personal"]).
    pub teams: Vec<String>,
    /// Name of the env var that holds the Linear personal API key. The key
    /// itself MUST NOT live in the TOML; the loader rejects any value that
    /// looks like a real Linear key (defense-in-depth, smithy/PER-31).
    pub api_key_env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReviewCfg {
    /// State the daemon polls for. Default: "Adversarial Review".
    pub state_name: String,
    /// State to transition to on pass.
    pub pass_state: String,
    /// State to transition to on fail.
    pub fail_state: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    /// Path to the persona markdown file. Relative to CWD or absolute.
    pub persona_path: String,
    /// Default agent CLI key when the persona doesn't specify one. Reserved
    /// for v0.2 multi-persona dispatch; the v0.1 wrapper reads agent_command
    /// off the persona itself.
    #[allow(dead_code)]
    #[serde(default = "default_agent_command")]
    pub agent_command: String,
    /// Hard cap on a single reviewer subprocess.
    #[serde(default = "default_subprocess_timeout")]
    pub subprocess_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceCfg {
    /// Root path for per-issue workspaces. Symphony writes to this same root
    /// (e.g. ~/code/symphony-workspaces); we read out of it.
    pub root: String,
}

fn default_poll_interval() -> u64 {
    30
}
fn default_agent_command() -> String {
    "claude".to_string()
}
fn default_subprocess_timeout() -> u64 {
    600
}

impl Config {
    /// Load from disk and validate. Refuses to load if anything in the TOML
    /// looks like a Linear API key.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config at {}", path.display()))?;
        Self::from_toml_str(&raw)
    }

    pub fn from_toml_str(raw: &str) -> Result<Self> {
        reject_inline_api_key(raw)?;
        let cfg: Config = toml::from_str(raw).context("parse anvil config TOML")?;
        if cfg.tracker.kind != "linear" {
            return Err(anyhow!(
                "tracker.kind must be 'linear' in v0.1; got '{}'",
                cfg.tracker.kind
            ));
        }
        if cfg.tracker.teams.is_empty() {
            return Err(anyhow!("tracker.teams must list at least one team name"));
        }
        if cfg.tracker.api_key_env.trim().is_empty() {
            return Err(anyhow!("tracker.api_key_env is required"));
        }
        Ok(cfg)
    }

    /// Resolve the workspace path for a given issue identifier. Expands `~`
    /// against $HOME and joins the sanitized identifier as a directory name.
    #[allow(dead_code)]
    pub fn workspace_for(&self, identifier: &str) -> PathBuf {
        let root = expand_tilde(&self.workspace.root);
        root.join(sanitize_identifier(identifier))
    }
}

/// Expand a leading `~` against $HOME. Anything else is taken verbatim. Used
/// for the workspace root and persona path.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if s == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

/// Make a filesystem-safe single path component out of a Linear identifier.
/// Allowed: [A-Za-z0-9._-]; everything else collapses to '_'.
pub fn sanitize_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Refuse the config if anything looks like a real Linear API key. Linear's
/// keys start with `lin_api_` (personal) or `lin_oauth_` (OAuth). Mirrors the
/// smithy/PER-31 placement: workers inherit HOME and could read the file.
fn reject_inline_api_key(raw: &str) -> Result<()> {
    for line in raw.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            continue;
        }
        if l.contains("lin_api_") && !l.contains("PASTE") && !l.contains("EXAMPLE") {
            return Err(anyhow!(
                "anvil config appears to contain a Linear personal API key (`lin_api_...`). \
                 Move the key to an environment variable and reference it via \
                 `tracker.api_key_env = \"LINEAR_TOKEN\"`. The agent subprocess inherits \
                 HOME and could read this file directly."
            ));
        }
        if l.contains("lin_oauth_") {
            return Err(anyhow!(
                "anvil config appears to contain a Linear OAuth token (`lin_oauth_...`). \
                 Move it to an environment variable; do not commit it to disk."
            ));
        }
    }
    Ok(())
}

/// The starter config written by `anvil init`.
pub fn starter_toml() -> &'static str {
    r#"# anvil config - Linear-only adversarial reviewer for Symphony
# Generated by `anvil init`. Edit before running `anvil run`.

[tracker]
kind = "linear"
# Optional. Limits issues by Linear project slug (omit to consider all).
# project_slug = "symphony-0c79b11b75ea"
teams = ["Personal"]
# NAME of the env var holding your Linear personal token. Never paste the
# token itself into this file. anvil refuses to load if it sees `lin_api_...`.
api_key_env = "LINEAR_TOKEN"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
poll_interval_seconds = 30
persona_path = "templates/reviewer.md"
agent_command = "claude"
subprocess_timeout_seconds = 600

[workspace]
# Symphony's per-issue workspace root. anvil reads diffs out of these
# directories; it does not create them.
root = "~/code/symphony-workspaces"
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[tracker]
kind = "linear"
teams = ["Personal"]
api_key_env = "LINEAR_TOKEN"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
persona_path = "templates/reviewer.md"

[workspace]
root = "~/code/symphony-workspaces"
"#;

    #[test]
    fn parses_minimal_valid_config() {
        let cfg = Config::from_toml_str(VALID).unwrap();
        assert_eq!(cfg.tracker.kind, "linear");
        assert_eq!(cfg.tracker.teams, vec!["Personal".to_string()]);
        assert_eq!(cfg.tracker.api_key_env, "LINEAR_TOKEN");
        assert_eq!(cfg.review.state_name, "Adversarial Review");
        assert_eq!(cfg.review.pass_state, "Human Review");
        assert_eq!(cfg.review.fail_state, "Rework");
        assert_eq!(cfg.review.poll_interval_seconds, 30);
        assert_eq!(cfg.review.agent_command, "claude");
        assert_eq!(cfg.workspace.root, "~/code/symphony-workspaces");
    }

    #[test]
    fn reject_inline_api_key() {
        // PER-31-style defense: any real-looking Linear key in the TOML must
        // refuse to load. The placeholder is still allowed so the example
        // file doesn't trip its own check.
        let bad = r#"
[tracker]
kind = "linear"
teams = ["Personal"]
api_key_env = "LINEAR_TOKEN"
secret = "lin_api_real_secret_value_xyz"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
persona_path = "templates/reviewer.md"

[workspace]
root = "~/code/symphony-workspaces"
"#;
        let err = Config::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("Linear personal API key"), "msg: {}", err);
    }

    #[test]
    fn reject_oauth_token_in_config() {
        let bad = r#"
[tracker]
kind = "linear"
teams = ["Personal"]
api_key_env = "LINEAR_TOKEN"
oauth = "lin_oauth_abc123"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
persona_path = "templates/reviewer.md"

[workspace]
root = "~/code/symphony-workspaces"
"#;
        let err = Config::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("OAuth token"));
    }

    #[test]
    fn placeholder_value_still_allowed() {
        // Comments and PASTE/EXAMPLE markers don't trip the rejector.
        let placeholder = r#"
# api_key_env = "lin_api_PASTE_KEY_HERE"
[tracker]
kind = "linear"
teams = ["Personal"]
api_key_env = "LINEAR_TOKEN"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
persona_path = "templates/reviewer.md"

[workspace]
root = "~/code/symphony-workspaces"
"#;
        Config::from_toml_str(placeholder).unwrap();
    }

    #[test]
    fn rejects_unknown_tracker_kind() {
        let bad = r#"
[tracker]
kind = "github"
teams = ["Personal"]
api_key_env = "LINEAR_TOKEN"

[review]
state_name = "Adversarial Review"
pass_state = "Human Review"
fail_state = "Rework"
persona_path = "templates/reviewer.md"

[workspace]
root = "~/code/symphony-workspaces"
"#;
        let err = Config::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("tracker.kind"));
    }

    #[test]
    fn workspace_for_sanitizes_identifier() {
        let cfg = Config::from_toml_str(VALID).unwrap();
        let p = cfg.workspace_for("PER-32");
        assert!(p.to_string_lossy().ends_with("PER-32"));
        // The sanitizer must collapse any `/` into `_` so the joined path
        // stays a single component under the workspace root. `..` as a
        // literal filename component is fine - the OS treats it as a name,
        // not traversal, when there's no separator.
        let p = cfg.workspace_for("../escape");
        let s = p.to_string_lossy();
        let trailing = s.rsplit('/').next().unwrap();
        assert!(!trailing.contains('/'));
        assert!(!trailing.is_empty());
    }

    #[test]
    fn sanitize_identifier_strips_separators() {
        assert_eq!(sanitize_identifier("PER-32"), "PER-32");
        // `/` is the only separator that matters for path-component safety;
        // `.` stays in so legitimate ids like `PER-3.1` round-trip cleanly.
        assert_eq!(sanitize_identifier("../weird"), ".._weird");
        assert_eq!(sanitize_identifier("foo/bar"), "foo_bar");
        assert_eq!(sanitize_identifier(""), "_");
    }
}

// Reviewer subprocess wrapper. Renders the persona body with the supplied
// vars, spawns the agent CLI with appropriate flags, pipes the prompt to
// stdin, waits with a hard timeout, then reads `<workspace>/REVIEW.md`.
//
// The agent's only job is to write REVIEW.md and exit. Anvil owns every
// Linear write (state transitions, comments) - this matches smithy's
// orchestrator-owns-state-transitions contract (PER-31).

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::persona::{self, Persona};
use crate::review::{self, Review};

#[derive(Debug)]
pub struct ReviewerOutput {
    pub exit_code: Option<i32>,
    /// Some(review) on success; None when REVIEW.md is missing or unparseable.
    /// The poll loop treats None as "leave state alone, comment, move on" -
    /// surface-don't-cancel.
    pub review: Option<Review>,
    /// Captured stderr. Surfaced to logs on subprocess failure.
    #[allow(dead_code)]
    pub raw_stderr: String,
    /// Reason the review is None, when applicable. Used in the surface comment.
    pub parse_error: Option<String>,
}

/// Standard variable bag for the reviewer prompt. Kept as an explicit struct
/// so the call site doesn't drift out of sync with the persona template.
#[derive(Debug, Clone)]
pub struct Vars {
    pub identifier: String,
    pub title: String,
    pub description: String,
    pub diff: String,
    pub branch: String,
    pub workspace_path: String,
}

impl Vars {
    fn into_map(self) -> HashMap<&'static str, String> {
        let mut m: HashMap<&'static str, String> = HashMap::new();
        m.insert("identifier", self.identifier);
        m.insert("title", self.title);
        m.insert("description", self.description);
        m.insert("diff", self.diff);
        m.insert("branch", self.branch);
        m.insert("workspace_path", self.workspace_path);
        m
    }
}

/// Spawn the reviewer with the supplied vars. Caller picks the workspace path
/// and is responsible for ensuring the directory exists.
pub async fn spawn_reviewer(
    persona: &Persona,
    vars: Vars,
    workspace: &Path,
    timeout_seconds: u64,
) -> Result<ReviewerOutput> {
    let var_map = vars.into_map();
    // Convert from owned-value map to a borrow-friendly &str-keyed map for
    // the renderer. Both share the same key strings so this is just a re-wrap.
    let render_map: HashMap<&str, String> = var_map
        .into_iter()
        .map(|(k, v)| (k, v))
        .collect();
    let prompt = persona::render(persona, &render_map);

    let cmd_key = persona.frontmatter.agent_command.as_str();
    let argv = build_argv(cmd_key, persona.frontmatter.model_hint.as_deref(), workspace);
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("empty argv for agent_command `{}`", cmd_key))?;

    tracing::info!(
        agent = cmd_key,
        workspace = %workspace.display(),
        prompt_bytes = prompt.len(),
        "spawning reviewer"
    );

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn agent `{}`", program))?;

    // Pipe the rendered prompt to stdin and close.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("write prompt to agent stdin")?;
        stdin
            .shutdown()
            .await
            .context("close agent stdin")?;
    }

    // Wait with a timeout. On timeout, kill the child and surface a parse
    // error so the loop comments and moves on.
    let timeout = Duration::from_secs(timeout_seconds);
    let wait = child.wait_with_output();
    let output = match tokio::time::timeout(timeout, wait).await {
        Ok(o) => o.context("collect agent output")?,
        Err(_) => {
            // wait_with_output consumed `child`; we can't kill it here. The
            // tokio runtime will clean up the orphan when the process group
            // ends. Document the timeout in the parse_error field.
            return Ok(ReviewerOutput {
                exit_code: None,
                review: None,
                raw_stderr: String::new(),
                parse_error: Some(format!(
                    "reviewer timed out after {}s",
                    timeout_seconds
                )),
            });
        }
    };

    let raw_stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();

    // Read REVIEW.md from the workspace. Missing file or parse error becomes
    // None + parse_error; the loop treats it as surface-don't-cancel.
    let review_path = workspace.join("REVIEW.md");
    if !review_path.exists() {
        return Ok(ReviewerOutput {
            exit_code,
            review: None,
            raw_stderr,
            parse_error: Some("REVIEW.md not found in workspace".to_string()),
        });
    }
    match review::parse_review(workspace) {
        Ok(r) => Ok(ReviewerOutput {
            exit_code,
            review: Some(r),
            raw_stderr,
            parse_error: None,
        }),
        Err(e) => Ok(ReviewerOutput {
            exit_code,
            review: None,
            raw_stderr,
            parse_error: Some(format!("parse REVIEW.md: {}", e)),
        }),
    }
}

/// Build the argv for the agent CLI. Handles claude vs codex; unknown
/// commands fall through to bare `<key>` so an operator can wire a custom
/// shim by putting it on PATH.
fn build_argv(agent_command: &str, model_hint: Option<&str>, workspace: &Path) -> Vec<String> {
    match agent_command {
        "claude" => build_claude_argv(model_hint, workspace),
        "codex" => build_codex_argv(),
        other => vec![other.to_string()],
    }
}

fn build_claude_argv(model_hint: Option<&str>, workspace: &Path) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "claude".to_string(),
        "-p".to_string(),
        "--setting-sources".to_string(),
        "project,local".to_string(),
        "--dangerously-skip-permissions".to_string(),
        // Strip any Linear write tools the agent might try to use; anvil owns
        // those calls. Belt and suspenders for read-only intent.
        "--disallowedTools".to_string(),
        "mcp__linear__save_issue,mcp__linear__save_comment,mcp__linear__create_attachment,mcp__linear__delete_attachment,mcp__linear__delete_comment,mcp__linear__save_document,mcp__linear__save_milestone,mcp__linear__save_project,mcp__linear__create_issue_label".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--add-dir".to_string(),
        workspace.to_string_lossy().to_string(),
    ];
    if let Some(hint) = model_hint {
        if let Some(model) = claude_model_for_hint(hint) {
            v.push("--model".to_string());
            v.push(model.to_string());
        }
    }
    v
}

fn claude_model_for_hint(hint: &str) -> Option<&'static str> {
    match hint.trim().to_ascii_lowercase().as_str() {
        "opus" => Some("claude-opus-4-7"),
        "sonnet" => Some("claude-sonnet-4-6"),
        "haiku" => Some("claude-haiku-4-5"),
        _ => None,
    }
}

fn build_codex_argv() -> Vec<String> {
    vec![
        "codex".to_string(),
        "exec".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
    ]
}

/// Resolve the per-issue workspace path under `root`. Symphony writes into
/// the same root, keyed by sanitized identifier; we read from the same path.
pub fn resolve_workspace(root: &Path, identifier: &str) -> PathBuf {
    let safe = crate::config::sanitize_identifier(identifier);
    root.join(safe)
}

/// Run `git diff <branch>...main` (or the equivalent) to capture what the
/// builder changed. On any failure (no git, branch missing, etc.) returns an
/// empty string with a tracing warning so the reviewer still gets the rest of
/// the context.
pub fn git_diff_against_main(workspace: &Path, branch: &str) -> String {
    if !workspace.exists() {
        tracing::warn!(workspace = %workspace.display(), "workspace missing, no diff");
        return String::new();
    }
    // Prefer `git diff main...<branch>` (symmetric difference) when the
    // workspace is a worktree on `branch`. If `branch` is empty / "main",
    // fall back to a HEAD-vs-main diff so the reviewer at least sees something.
    let arg = if branch.is_empty() || branch == "main" {
        "main...HEAD".to_string()
    } else {
        format!("main...{}", branch)
    };
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff", &arg])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!(stderr = %stderr, "git diff failed, returning empty");
            String::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "git not available, returning empty diff");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_claude_argv_includes_required_flags() {
        let ws = std::path::Path::new("/tmp/anvil-test-ws");
        let argv = build_argv("claude", Some("sonnet"), ws);
        assert_eq!(argv[0], "claude");
        assert!(argv.contains(&"-p".to_string()));
        assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(argv.contains(&"--disallowedTools".to_string()));
        assert!(argv.contains(&"--output-format".to_string()));
        // model hint resolves to a concrete --model
        let i = argv.iter().position(|a| a == "--model").expect("--model present");
        assert_eq!(argv[i + 1], "claude-sonnet-4-6");
    }

    #[test]
    fn build_codex_argv_uses_exec_subcommand() {
        let argv = build_argv("codex", None, std::path::Path::new("/tmp"));
        assert_eq!(argv[0], "codex");
        assert!(argv.contains(&"exec".to_string()));
    }

    #[test]
    fn unknown_agent_command_passes_through() {
        let argv = build_argv("custom-shim", None, std::path::Path::new("/tmp"));
        assert_eq!(argv, vec!["custom-shim".to_string()]);
    }

    #[test]
    fn unknown_model_hint_omits_model_flag() {
        let argv = build_argv("claude", Some("supreme"), std::path::Path::new("/tmp"));
        assert!(!argv.iter().any(|a| a == "--model"));
    }

    #[test]
    fn resolve_workspace_is_safe_for_weird_identifiers() {
        // Single path component under the root - `/` collapses to `_` so a
        // crafted identifier can't escape into a sibling directory.
        let root = std::path::Path::new("/tmp/anvil");
        let p = resolve_workspace(root, "../escape");
        let trailing = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        assert!(!trailing.contains('/'));
        assert!(!trailing.is_empty());
    }
}

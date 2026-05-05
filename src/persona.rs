// Persona loader: a markdown file with YAML frontmatter that describes how to
// invoke an agent and a body that becomes the prompt template.
//
// The persona pattern is the seed for everything anvil builds: future personas
// (Penny meeting-attender, Argyle code-reviewer, anything else) follow the same
// shape - frontmatter-meta + markdown-body + {{var}} placeholders. The wrapper
// only needs to know the agent_command; the body is opaque text.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Parsed persona file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    pub frontmatter: PersonaMeta,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PersonaMeta {
    pub name: String,
    pub description: String,
    /// CLI key for the agent to spawn. Either "claude" or "codex" in v0.1.
    /// The wrapper maps this to a real argv in `worker::build_argv`.
    pub agent_command: String,
    /// Optional model hint. For `claude`, "sonnet" / "opus" / "haiku" map to
    /// concrete --model values. For `codex`, currently advisory only.
    #[serde(default)]
    pub model_hint: Option<String>,
}

/// Read a persona file off disk and split frontmatter from body.
pub fn load_persona(path: &Path) -> Result<Persona> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read persona at {}", path.display()))?;
    parse_persona(&content)
}

/// In-memory parse. Same logic as `load_persona`, exposed for tests so they
/// don't need a tempfile.
pub fn parse_persona(content: &str) -> Result<Persona> {
    let (yaml, body) = split_frontmatter(content)?;
    let frontmatter: PersonaMeta = serde_yaml::from_str(&yaml)
        .with_context(|| "parse persona YAML frontmatter")?;
    if frontmatter.name.trim().is_empty() {
        return Err(anyhow!("persona frontmatter `name` is required"));
    }
    if frontmatter.agent_command.trim().is_empty() {
        return Err(anyhow!("persona frontmatter `agent_command` is required"));
    }
    Ok(Persona { frontmatter, body })
}

/// Render the persona body by replacing `{{var}}` occurrences with the value
/// from `vars`. Unknown keys are left in place so missing-var bugs surface in
/// the agent's prompt rather than silently disappearing.
pub fn render(persona: &Persona, vars: &HashMap<&str, String>) -> String {
    render_str(&persona.body, vars)
}

pub fn render_str(template: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(close) = find_close(bytes, i + 2) {
                let key = std::str::from_utf8(&bytes[i + 2..close])
                    .unwrap_or("")
                    .trim();
                if let Some(v) = vars.get(key) {
                    out.push_str(v);
                    i = close + 2;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn find_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Split the leading `---` YAML fences off a markdown document. Skips blank
/// lines before the opening fence so editors that auto-prefix a newline don't
/// trip the parser. Returns (yaml, body).
fn split_frontmatter(content: &str) -> Result<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return Err(anyhow!("persona file is empty"));
    }
    if lines[i].trim() != "---" {
        return Err(anyhow!(
            "expected `---` at start of persona frontmatter, got `{}`",
            lines[i]
        ));
    }
    i += 1;
    let mut yaml: Vec<&str> = Vec::new();
    while i < lines.len() {
        if lines[i].trim() == "---" {
            let body_lines: Vec<&str> = lines[i + 1..].to_vec();
            return Ok((yaml.join("\n"), body_lines.join("\n")));
        }
        yaml.push(lines[i]);
        i += 1;
    }
    Err(anyhow!("persona frontmatter missing closing `---`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"---
name: anvil-reviewer
description: Adversarial code reviewer
agent_command: claude
model_hint: sonnet
---
You are reviewing {{identifier}}: {{title}}.

The diff is below.
{{diff}}
"#;

    #[test]
    fn load_minimal_persona() {
        let p = parse_persona(MIN).unwrap();
        assert_eq!(p.frontmatter.name, "anvil-reviewer");
        assert_eq!(p.frontmatter.description, "Adversarial code reviewer");
        assert_eq!(p.frontmatter.agent_command, "claude");
        assert_eq!(p.frontmatter.model_hint.as_deref(), Some("sonnet"));
        assert!(p.body.starts_with("You are reviewing"));
        assert!(p.body.contains("{{diff}}"));
    }

    #[test]
    fn render_with_vars() {
        let p = parse_persona(MIN).unwrap();
        let mut vars: HashMap<&str, String> = HashMap::new();
        vars.insert("identifier", "PER-99".into());
        vars.insert("title", "fix the thing".into());
        vars.insert("diff", "@@ -1 +1 @@\n- old\n+ new\n".into());
        let out = render(&p, &vars);
        assert!(out.contains("PER-99: fix the thing"));
        assert!(out.contains("+ new"));
        assert!(!out.contains("{{identifier}}"));
        assert!(!out.contains("{{diff}}"));
    }

    #[test]
    fn render_leaves_unknown_keys_in_place() {
        // Bug-surfacing default: a typo'd placeholder shows up in the rendered
        // prompt rather than disappearing silently.
        let p = parse_persona(MIN).unwrap();
        let vars: HashMap<&str, String> = HashMap::new();
        let out = render(&p, &vars);
        assert!(out.contains("{{identifier}}"));
        assert!(out.contains("{{title}}"));
    }

    #[test]
    fn rejects_persona_without_frontmatter() {
        let raw = "no fences here, just markdown\n";
        assert!(parse_persona(raw).is_err());
    }

    #[test]
    fn rejects_persona_with_unclosed_frontmatter() {
        let raw = "---\nname: x\nagent_command: y\n";
        let err = parse_persona(raw).unwrap_err().to_string();
        assert!(err.contains("closing"));
    }

    #[test]
    fn rejects_missing_required_fields() {
        let raw = "---\nname: \"\"\ndescription: x\nagent_command: claude\n---\n";
        let err = parse_persona(raw).unwrap_err().to_string();
        assert!(err.contains("name"));
    }

    #[test]
    fn skips_leading_blank_lines() {
        let raw = "\n\n---\nname: x\ndescription: y\nagent_command: claude\n---\nbody\n";
        let p = parse_persona(raw).unwrap();
        assert_eq!(p.frontmatter.name, "x");
    }
}

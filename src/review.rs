// REVIEW.md parser. The reviewer agent writes this file at the workspace root
// and exits; anvil parses it and decides the Linear transition.
//
// Schema: YAML frontmatter (between `---` fences) with at least a `status`
// field (pass|fail), an optional `findings` list of `{finding, grade}` items,
// and optional free-form `notes`. status=fail requires at least one Blocker
// finding; otherwise the parser returns Malformed and the daemon should leave
// the issue alone for the operator (we don't auto-retry; that's symphony's job).

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grade {
    Blocker,
    Polish,
    Future,
}

impl Grade {
    pub fn as_str(&self) -> &'static str {
        match self {
            Grade::Blocker => "blocker",
            Grade::Polish => "polish",
            Grade::Future => "future",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub finding: String,
    pub grade: Grade,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub status: ReviewStatus,
    pub findings: Vec<Finding>,
    pub notes: String,
}

#[derive(Debug, Deserialize)]
struct RawFinding {
    finding: String,
    grade: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    status: Option<String>,
    #[serde(default)]
    findings: Vec<RawFinding>,
    #[serde(default)]
    notes: String,
}

/// Load and parse `<workspace>/REVIEW.md`.
pub fn parse_review(workspace: &Path) -> Result<Review> {
    let path = workspace.join("REVIEW.md");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("read REVIEW.md at {}: {}", path.display(), e))?;
    parse_review_str(&content)
}

/// Parse a REVIEW.md string. Exposed for tests.
pub fn parse_review_str(content: &str) -> Result<Review> {
    let yaml = split_frontmatter(content)?;
    let raw: RawFrontmatter = serde_yaml::from_str(&yaml)
        .map_err(|e| anyhow!("parse REVIEW.md frontmatter: {}", e))?;
    let status = match raw.status.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s == "pass" => ReviewStatus::Pass,
        Some(s) if s == "fail" => ReviewStatus::Fail,
        Some(other) => {
            return Err(anyhow!(
                "unknown status `{}`; expected pass|fail",
                other
            ))
        }
        None => return Err(anyhow!("REVIEW.md missing required `status` field")),
    };
    let mut findings = Vec::with_capacity(raw.findings.len());
    for rf in raw.findings {
        let grade = match rf.grade.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
            Some(g) if g == "blocker" => Grade::Blocker,
            Some(g) if g == "polish" => Grade::Polish,
            Some(g) if g == "future" => Grade::Future,
            Some(other) => {
                return Err(anyhow!(
                    "unknown grade `{}`; expected blocker|polish|future",
                    other
                ))
            }
            None => return Err(anyhow!("finding missing required `grade` field")),
        };
        findings.push(Finding {
            finding: rf.finding,
            grade,
        });
    }
    let has_blocker = findings.iter().any(|f| f.grade == Grade::Blocker);
    if status == ReviewStatus::Fail && !has_blocker {
        return Err(anyhow!(
            "status=fail requires at least one finding with grade=blocker"
        ));
    }
    Ok(Review {
        status,
        findings,
        notes: raw.notes,
    })
}

fn split_frontmatter(content: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return Err(anyhow!("REVIEW.md is empty"));
    }
    if lines[i].trim() != "---" {
        return Err(anyhow!(
            "expected `---` at start of REVIEW.md, got `{}`",
            lines[i]
        ));
    }
    i += 1;
    let mut fm: Vec<&str> = Vec::new();
    while i < lines.len() {
        if lines[i].trim() == "---" {
            return Ok(fm.join("\n"));
        }
        fm.push(lines[i]);
        i += 1;
    }
    Err(anyhow!("REVIEW.md missing closing `---`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_workspace(label: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "anvil-review-test-{}-{}-{}",
            std::process::id(),
            n,
            label
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parse_pass_no_findings() {
        let ws = tmp_workspace("pass-clean");
        fs::write(
            ws.join("REVIEW.md"),
            "---\nstatus: pass\nfindings: []\nnotes: ship it\n---\n",
        )
        .unwrap();
        let r = parse_review(&ws).unwrap();
        assert_eq!(r.status, ReviewStatus::Pass);
        assert!(r.findings.is_empty());
        assert_eq!(r.notes, "ship it");
    }

    #[test]
    fn parse_pass_with_advisory_findings() {
        // Pass-with-polish/future is the common shape: reviewer flagged
        // something but it's not blocker-grade. Anvil should still pass and
        // surface the notes.
        let ws = tmp_workspace("pass-advisory");
        fs::write(
            ws.join("REVIEW.md"),
            "---\nstatus: pass\nfindings:\n  - finding: rename helper\n    grade: polish\n  - finding: add stress test\n    grade: future\nnotes: clean enough to ship\n---\n",
        )
        .unwrap();
        let r = parse_review(&ws).unwrap();
        assert_eq!(r.status, ReviewStatus::Pass);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0].grade, Grade::Polish);
        assert_eq!(r.findings[1].grade, Grade::Future);
    }

    #[test]
    fn parse_fail_with_blocker() {
        let ws = tmp_workspace("fail-blocker");
        fs::write(
            ws.join("REVIEW.md"),
            "---\nstatus: fail\nfindings:\n  - finding: parser panics on empty input\n    grade: blocker\n  - finding: nit on naming\n    grade: polish\nnotes: send back\n---\n",
        )
        .unwrap();
        let r = parse_review(&ws).unwrap();
        assert_eq!(r.status, ReviewStatus::Fail);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0].grade, Grade::Blocker);
        assert_eq!(r.notes, "send back");
    }

    #[test]
    fn parse_malformed_returns_error() {
        // No frontmatter at all -> error.
        assert!(parse_review_str("just some markdown\n").is_err());

        // Unknown grade -> error.
        let bad_grade = "---\nstatus: fail\nfindings:\n  - finding: x\n    grade: spicy\n---\n";
        assert!(parse_review_str(bad_grade).is_err());

        // Missing grade -> error.
        let no_grade = "---\nstatus: fail\nfindings:\n  - finding: y\n---\n";
        assert!(parse_review_str(no_grade).is_err());

        // Unknown status -> error.
        let bad_status = "---\nstatus: maybe\n---\n";
        assert!(parse_review_str(bad_status).is_err());

        // status=fail without any blocker -> error (parser refuses to
        // upgrade polish to blocker; fixing this is the agent's job).
        let fail_no_blocker = "---\nstatus: fail\nfindings:\n  - finding: x\n    grade: polish\n---\n";
        let err = parse_review_str(fail_no_blocker).unwrap_err().to_string();
        assert!(err.contains("blocker"));
    }
}

// Minimal Linear GraphQL client. Direct HTTP via reqwest, no MCP - the daemon
// runs unattended and needs stable auth. Modeled on smithy/linear.rs but
// trimmed to the calls anvil actually makes.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: String,
    /// Linear's `gitBranchName` field. None for issues without a branch (rare
    /// but possible). Diff fallback when None: HEAD vs main.
    pub branch_name: Option<String>,
    pub team_id: String,
    pub team_name: String,
}

#[derive(Clone)]
pub struct LinearClient {
    api_key: String,
    http: reqwest::Client,
    endpoint: String,
}

impl LinearClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            http: reqwest::Client::builder()
                .user_agent("anvil/0.1")
                .build()
                .expect("reqwest client"),
            endpoint: LINEAR_GRAPHQL_URL.to_string(),
        }
    }

    /// Test hook: override the endpoint. Used by mocked transport tests.
    #[allow(dead_code)]
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    async fn graphql<V: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: V,
    ) -> Result<T> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let resp = self
            .http
            .post(&self.endpoint)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("linear graphql request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read linear response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "linear graphql HTTP {} body: {}",
                status,
                truncate(&text, 500)
            ));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("parse linear response as JSON")?;
        if let Some(errors) = parsed.get("errors") {
            return Err(anyhow!("linear graphql errors: {}", errors));
        }
        let data = parsed.get("data").ok_or_else(|| {
            anyhow!(
                "linear response missing data field: {}",
                truncate(&text, 500)
            )
        })?;
        let typed: T = serde_json::from_value(data.clone())
            .context("deserialize linear data field")?;
        Ok(typed)
    }

    /// Sanity-check the API key by reading viewer.email. Used by `anvil check`.
    pub async fn viewer_email(&self) -> Result<String> {
        let query = r#"query { viewer { email } }"#;
        #[derive(Deserialize)]
        struct Resp {
            viewer: V,
        }
        #[derive(Deserialize)]
        struct V {
            email: String,
        }
        let r: Resp = self.graphql(query, serde_json::json!({})).await?;
        Ok(r.viewer.email)
    }

    /// List issues in `state_name` belonging to any of `team_names`. Optional
    /// `project_slug` filter limits to a single Linear project (matches by
    /// slug-id; e.g. "symphony-0c79b11b75ea").
    pub async fn fetch_issues_in_state(
        &self,
        team_names: &[String],
        state_name: &str,
        project_slug: Option<&str>,
    ) -> Result<Vec<Issue>> {
        let query = r#"
query AnvilFetch($teamNames: [String!]!, $state: String!, $after: String) {
  issues(
    first: 50
    after: $after
    filter: {
      team: { name: { in: $teamNames } }
      state: { name: { eq: $state } }
    }
  ) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      identifier
      title
      description
      branchName
      team { id name }
      project { slugId }
    }
  }
}
"#;
        let mut out: Vec<Issue> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let vars = serde_json::json!({
                "teamNames": team_names,
                "state": state_name,
                "after": after,
            });
            #[derive(Deserialize)]
            struct Resp {
                issues: Page,
            }
            #[derive(Deserialize)]
            struct Page {
                #[serde(rename = "pageInfo")]
                page_info: PageInfo,
                nodes: Vec<RawIssue>,
            }
            #[derive(Deserialize)]
            struct PageInfo {
                #[serde(rename = "hasNextPage")]
                has_next_page: bool,
                #[serde(rename = "endCursor")]
                end_cursor: Option<String>,
            }
            #[derive(Deserialize)]
            struct RawIssue {
                id: String,
                identifier: String,
                title: String,
                #[serde(default)]
                description: Option<String>,
                #[serde(rename = "branchName", default)]
                branch_name: Option<String>,
                team: TeamRef,
                #[serde(default)]
                project: Option<ProjectRef>,
            }
            #[derive(Deserialize)]
            struct TeamRef {
                id: String,
                name: String,
            }
            #[derive(Deserialize)]
            struct ProjectRef {
                #[serde(rename = "slugId", default)]
                slug_id: Option<String>,
            }

            let resp: Resp = self.graphql(query, vars).await?;
            for raw in resp.issues.nodes {
                if let Some(slug) = project_slug {
                    let raw_slug = raw.project.as_ref().and_then(|p| p.slug_id.as_deref());
                    if raw_slug != Some(slug) {
                        continue;
                    }
                }
                out.push(Issue {
                    id: raw.id,
                    identifier: raw.identifier,
                    title: raw.title,
                    description: raw.description.unwrap_or_default(),
                    branch_name: raw.branch_name,
                    team_id: raw.team.id,
                    team_name: raw.team.name,
                });
            }
            if !resp.issues.page_info.has_next_page {
                break;
            }
            after = resp.issues.page_info.end_cursor;
            if after.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Resolve a workflow state name to its UUID within a team. Linear scopes
    /// state IDs per team, so we need the team to disambiguate.
    pub async fn fetch_state_id(
        &self,
        team_name: &str,
        state_name: &str,
    ) -> Result<String> {
        let query = r#"
query AnvilState($team: String!) {
  workflowStates(filter: { team: { name: { eq: $team } } }, first: 50) {
    nodes { id name }
  }
}
"#;
        let vars = serde_json::json!({ "team": team_name });
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "workflowStates")]
            workflow_states: WS,
        }
        #[derive(Deserialize)]
        struct WS {
            nodes: Vec<RawState>,
        }
        #[derive(Deserialize)]
        struct RawState {
            id: String,
            name: String,
        }
        let resp: Resp = self.graphql(query, vars).await?;
        resp.workflow_states
            .nodes
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(state_name))
            .map(|s| s.id)
            .ok_or_else(|| {
                anyhow!(
                    "workflow state `{}` not found in team `{}`",
                    state_name,
                    team_name
                )
            })
    }

    /// Move an issue to `state_name` within its owning team.
    pub async fn transition_state(
        &self,
        issue_id: &str,
        team_name: &str,
        state_name: &str,
    ) -> Result<()> {
        let state_id = self.fetch_state_id(team_name, state_name).await?;
        let mutation = r#"
mutation AnvilUpdate($id: String!, $stateId: String!) {
  issueUpdate(id: $id, input: { stateId: $stateId }) { success }
}
"#;
        let vars = serde_json::json!({ "id": issue_id, "stateId": state_id });
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "issueUpdate")]
            update: U,
        }
        #[derive(Deserialize)]
        struct U {
            success: bool,
        }
        let resp: Resp = self.graphql(mutation, vars).await?;
        if !resp.update.success {
            return Err(anyhow!("issueUpdate returned success=false"));
        }
        Ok(())
    }

    /// Post a markdown comment on an issue. Authorship is whoever owns the
    /// API key; v0.1 doesn't split orchestrator vs reviewer attribution.
    pub async fn add_comment(&self, issue_id: &str, body: &str) -> Result<()> {
        let mutation = r#"
mutation AnvilComment($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) { success }
}
"#;
        let vars = serde_json::json!({ "issueId": issue_id, "body": body });
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "commentCreate")]
            cc: C,
        }
        #[derive(Deserialize)]
        struct C {
            success: bool,
        }
        let resp: Resp = self.graphql(mutation, vars).await?;
        if !resp.cc.success {
            return Err(anyhow!("commentCreate returned success=false"));
        }
        Ok(())
    }

    /// Find the persistent Symphony/Codex workpad comment on an issue. Returns
    /// `(comment_id, body)` for the first comment whose body starts with
    /// `## Codex Workpad`. Returns `Ok(None)` when no workpad exists, which
    /// is normal for issues that the build worker never touched (e.g. operator
    /// drag straight to Adversarial Review for a manual demo).
    pub async fn find_workpad_comment(
        &self,
        issue_id: &str,
    ) -> Result<Option<(String, String)>> {
        let query = r#"
query AnvilWorkpad($issueId: String!) {
  issue(id: $issueId) {
    comments(first: 50) {
      nodes { id body }
    }
  }
}
"#;
        let vars = serde_json::json!({ "issueId": issue_id });
        #[derive(Deserialize)]
        struct Resp {
            issue: Option<IssueWrap>,
        }
        #[derive(Deserialize)]
        struct IssueWrap {
            comments: CommentsWrap,
        }
        #[derive(Deserialize)]
        struct CommentsWrap {
            nodes: Vec<RawComment>,
        }
        #[derive(Deserialize)]
        struct RawComment {
            id: String,
            body: String,
        }
        let resp: Resp = self.graphql(query, vars).await?;
        let nodes = resp
            .issue
            .map(|i| i.comments.nodes)
            .unwrap_or_default();
        Ok(nodes
            .into_iter()
            .find(|c| c.body.trim_start().starts_with("## Codex Workpad"))
            .map(|c| (c.id, c.body)))
    }

    /// Replace the body of an existing comment. Used to append review verdicts
    /// to the workpad without spawning new comment threads.
    pub async fn update_comment(&self, comment_id: &str, body: &str) -> Result<()> {
        let mutation = r#"
mutation AnvilUpdateComment($id: String!, $body: String!) {
  commentUpdate(id: $id, input: { body: $body }) { success }
}
"#;
        let vars = serde_json::json!({ "id": comment_id, "body": body });
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "commentUpdate")]
            cu: U,
        }
        #[derive(Deserialize)]
        struct U {
            success: bool,
        }
        let resp: Resp = self.graphql(mutation, vars).await?;
        if !resp.cu.success {
            return Err(anyhow!("commentUpdate returned success=false"));
        }
        Ok(())
    }

    /// Append `content` as a dated `#### {timestamp} {label}` subsection under
    /// `### {section}` inside the workpad comment. If the workpad doesn't
    /// exist on this issue, falls back to creating a standalone comment with
    /// the same content. This preserves Symphony's single-thread workpad
    /// pattern instead of fragmenting review history across multiple comments.
    pub async fn append_to_workpad_or_create(
        &self,
        issue_id: &str,
        section: &str,
        label: &str,
        content: &str,
    ) -> Result<()> {
        let stamp = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        match self.find_workpad_comment(issue_id).await? {
            Some((cid, body)) => {
                let new_body = append_dated_subsection(&body, section, &stamp, label, content);
                self.update_comment(&cid, &new_body).await
            }
            None => {
                let standalone = format!(
                    "## anvil (no workpad found)\n\n### {}\n\n#### {} {}\n\n{}",
                    section,
                    stamp,
                    label,
                    content.trim()
                );
                self.add_comment(issue_id, &standalone).await
            }
        }
    }
}

/// Insert a new `#### {stamp} {label}` subsection plus its body under the
/// `### {section}` H3 inside `body`. If the H3 already exists, the new
/// subsection is placed immediately before the next H3 (so it stays grouped
/// under the right header, even when the workpad has trailing sections like
/// `### Confusions`). If the H3 is absent, it's created at the end of the
/// document with the new subsection inside.
pub(crate) fn append_dated_subsection(
    body: &str,
    section: &str,
    stamp: &str,
    label: &str,
    content: &str,
) -> String {
    let header = format!("### {}", section);
    let new_block = format!("#### {} {}\n\n{}", stamp, label, content.trim());

    if let Some(h3_start) = find_section_header(body, &header) {
        // Find the next "\n### " after the H3's content.
        let scan_from = h3_start + header.len();
        let next_h3 = body[scan_from..]
            .find("\n### ")
            .map(|off| scan_from + off);
        let insertion_point = next_h3.unwrap_or_else(|| body.trim_end().len());
        let head = body[..insertion_point].trim_end();
        let tail = &body[insertion_point..];
        format!("{}\n\n{}\n{}", head, new_block, tail.trim_start_matches('\n'))
    } else {
        format!(
            "{}\n\n{}\n\n{}\n",
            body.trim_end(),
            header,
            new_block
        )
    }
}

/// Find a markdown H3 header line as a complete line. Avoids matching prefixes
/// like `### Plan` when searching for `### Pl`, and skips `#### ` H4 lines.
fn find_section_header(body: &str, header: &str) -> Option<usize> {
    let needle = header;
    let mut search_from = 0usize;
    while let Some(found) = body[search_from..].find(needle) {
        let abs = search_from + found;
        // Must be at start-of-line (or start-of-string).
        let at_line_start = abs == 0 || body.as_bytes()[abs - 1] == b'\n';
        if at_line_start {
            // Must be exactly "### " not "#### ".
            let after = abs + needle.len();
            let next_byte = body.as_bytes().get(after).copied();
            if matches!(next_byte, Some(b'\n') | Some(b' ') | None) {
                return Some(abs);
            }
        }
        search_from = abs + needle.len();
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = "2026-05-08 14:30 UTC";

    #[test]
    fn appends_section_when_absent() {
        let body = "## Codex Workpad\n\n### Plan\n\n- [x] do the thing\n";
        let out = append_dated_subsection(body, "Adversarial Review", STAMP, "PASS", "looks good");
        assert!(out.contains("### Adversarial Review"));
        assert!(out.contains("#### 2026-05-08 14:30 UTC PASS"));
        assert!(out.contains("looks good"));
        // Plan section preserved
        assert!(out.contains("### Plan"));
    }

    #[test]
    fn appends_subsection_to_existing_section() {
        let body = "## Codex Workpad\n\n### Adversarial Review\n\n#### 2026-05-08 14:00 UTC FAIL\n\nmissing tests\n";
        let out = append_dated_subsection(
            body,
            "Adversarial Review",
            STAMP,
            "PASS",
            "tests added",
        );
        // Both subsections present
        assert!(out.contains("#### 2026-05-08 14:00 UTC FAIL"));
        assert!(out.contains("#### 2026-05-08 14:30 UTC PASS"));
        // Only one H3 for Adversarial Review (no duplicate header)
        assert_eq!(
            out.matches("### Adversarial Review").count(),
            1,
            "should not duplicate the section header"
        );
    }

    #[test]
    fn inserts_before_next_h3_when_section_has_trailing_sections() {
        let body = "## Codex Workpad\n\n### Adversarial Review\n\n#### 2026-05-08 14:00 UTC FAIL\n\nmissing tests\n\n### Confusions\n\n- none\n";
        let out = append_dated_subsection(
            body,
            "Adversarial Review",
            STAMP,
            "PASS",
            "tests added",
        );
        // The new subsection lands BEFORE Confusions, not after.
        let pass_idx = out.find("#### 2026-05-08 14:30 UTC PASS").unwrap();
        let confusions_idx = out.find("### Confusions").unwrap();
        assert!(
            pass_idx < confusions_idx,
            "new pass subsection must be grouped under Adversarial Review, before Confusions"
        );
        // Confusions still present
        assert!(out.contains("### Confusions"));
    }

    #[test]
    fn does_not_match_h4_as_h3() {
        // A workpad whose Notes section happens to mention "### Adversarial Review"
        // inside a code fence or quoted block should not be confused for the real
        // header. We approximate this with an H4 line that contains the same text.
        let body = "## Codex Workpad\n\n### Notes\n\n#### Adversarial Review (planning note)\n\nnot the real section\n";
        let out = append_dated_subsection(body, "Adversarial Review", STAMP, "PASS", "ship it");
        // Should create a fresh H3 at end since real header is absent.
        assert!(
            out.contains("\n### Adversarial Review\n"),
            "should append a real H3 since the H4 doesn't count as the section header"
        );
    }

    #[test]
    fn handles_missing_workpad_body_gracefully() {
        let body = "";
        let out = append_dated_subsection(body, "Adversarial Review", STAMP, "PASS", "ok");
        assert!(out.contains("### Adversarial Review"));
        assert!(out.contains("#### 2026-05-08 14:30 UTC PASS"));
    }
}

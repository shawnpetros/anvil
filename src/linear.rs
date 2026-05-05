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
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n])
    }
}

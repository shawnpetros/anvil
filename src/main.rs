// anvil: adversarial pre-PR reviewer that bolts onto OpenAI Symphony.
//
// One new Linear state, "Adversarial Review", inserted between Symphony's
// "In Progress" and "Human Review". Symphony's agent finishes, transitions to
// Adversarial Review, anvil polls, runs a cross-model reviewer, then either
// passes (-> Human Review) or fails (-> Rework with findings as a comment).

mod config;
mod linear;
mod persona;
mod review;
mod worker;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "anvil", version = VERSION, about = "adversarial pre-PR reviewer for Symphony")]
struct Cli {
    /// Path to anvil's TOML config. Defaults to ~/.config/anvil/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Write a starter config to ~/.config/anvil/config.toml if missing.
    Init,
    /// Verify config + Linear connectivity, print summary, exit.
    Check,
    /// Start the poll loop. Runs until SIGINT.
    Run,
    /// Print version and exit.
    Version,
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".config").join("anvil").join("config.toml"))
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = match cli.config {
        Some(p) => p,
        None => default_config_path()?,
    };
    match cli.cmd {
        Cmd::Version => {
            println!("anvil {}", VERSION);
            Ok(())
        }
        Cmd::Init => cmd_init(&cfg_path),
        Cmd::Check => {
            init_tracing();
            cmd_check(&cfg_path).await
        }
        Cmd::Run => {
            init_tracing();
            cmd_run(&cfg_path).await
        }
    }
}

fn cmd_init(cfg_path: &std::path::Path) -> Result<()> {
    if cfg_path.exists() {
        println!("config already exists at {}", cfg_path.display());
        return Ok(());
    }
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(cfg_path, config::starter_toml())
        .with_context(|| format!("write {}", cfg_path.display()))?;
    println!("wrote starter config to {}", cfg_path.display());
    println!("next: set the env var named in `tracker.api_key_env` (default LINEAR_TOKEN), then `anvil check`");
    Ok(())
}

async fn cmd_check(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let api_key = std::env::var(&cfg.tracker.api_key_env).map_err(|_| {
        anyhow!(
            "env var `{}` not set; export your Linear personal token before running anvil",
            cfg.tracker.api_key_env
        )
    })?;
    if api_key.trim().is_empty() {
        return Err(anyhow!(
            "env var `{}` is empty",
            cfg.tracker.api_key_env
        ));
    }
    let client = linear::LinearClient::new(api_key);
    let email = client
        .viewer_email()
        .await
        .context("Linear viewer query failed; check your token")?;
    println!("anvil check OK");
    println!("  config:        {}", cfg_path.display());
    println!("  linear viewer: {}", email);
    println!("  teams:         {}", cfg.tracker.teams.join(", "));
    println!("  watching:      `{}`", cfg.review.state_name);
    println!("  pass ->        `{}`", cfg.review.pass_state);
    println!("  fail ->        `{}`", cfg.review.fail_state);
    println!("  workspace:     {}", cfg.workspace.root);
    Ok(())
}

async fn cmd_run(cfg_path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(cfg_path)?;
    let api_key = std::env::var(&cfg.tracker.api_key_env).map_err(|_| {
        anyhow!(
            "env var `{}` not set; export your Linear personal token before running anvil",
            cfg.tracker.api_key_env
        )
    })?;
    if api_key.trim().is_empty() {
        return Err(anyhow!("env var `{}` is empty", cfg.tracker.api_key_env));
    }
    let persona_path = config::expand_tilde(&cfg.review.persona_path);
    let persona = persona::load_persona(&persona_path)?;
    tracing::info!(
        persona = %persona.frontmatter.name,
        agent = %persona.frontmatter.agent_command,
        teams = ?cfg.tracker.teams,
        watching = %cfg.review.state_name,
        "anvil starting"
    );
    let client = linear::LinearClient::new(api_key);
    run_loop(cfg, persona, client).await
}

async fn run_loop(
    cfg: config::Config,
    persona: persona::Persona,
    client: linear::LinearClient,
) -> Result<()> {
    let workspace_root = config::expand_tilde(&cfg.workspace.root);
    loop {
        match poll_once(&cfg, &persona, &client, &workspace_root).await {
            Ok(processed) => {
                if processed > 0 {
                    tracing::info!(processed, "review batch complete");
                }
            }
            Err(e) => {
                // surface-don't-cancel: log the error, sleep, try again.
                tracing::error!(error = %e, "poll iteration failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(cfg.review.poll_interval_seconds)).await;
    }
}

async fn poll_once(
    cfg: &config::Config,
    persona: &persona::Persona,
    client: &linear::LinearClient,
    workspace_root: &std::path::Path,
) -> Result<usize> {
    let issues = client
        .fetch_issues_in_state(
            &cfg.tracker.teams,
            &cfg.review.state_name,
            cfg.tracker.project_slug.as_deref(),
        )
        .await
        .context("fetch issues in adversarial-review state")?;
    if issues.is_empty() {
        return Ok(0);
    }
    let mut count = 0usize;
    for issue in issues {
        if let Err(e) = review_one(cfg, persona, client, workspace_root, &issue).await {
            tracing::error!(issue = %issue.identifier, error = %e, "review_one failed");
        }
        count += 1;
    }
    Ok(count)
}

async fn review_one(
    cfg: &config::Config,
    persona: &persona::Persona,
    client: &linear::LinearClient,
    workspace_root: &std::path::Path,
    issue: &linear::Issue,
) -> Result<()> {
    let workspace = worker::resolve_workspace(workspace_root, &issue.identifier);
    let branch = issue.branch_name.clone().unwrap_or_else(|| "main".to_string());
    let diff = worker::git_diff_against_main(&workspace, &branch);

    tracing::info!(
        issue = %issue.identifier,
        workspace = %workspace.display(),
        branch = %branch,
        diff_bytes = diff.len(),
        "running reviewer"
    );

    let vars = worker::Vars {
        identifier: issue.identifier.clone(),
        title: issue.title.clone(),
        description: issue.description.clone(),
        diff,
        branch: branch.clone(),
        workspace_path: workspace.to_string_lossy().to_string(),
    };

    let output = worker::spawn_reviewer(
        persona,
        vars,
        &workspace,
        cfg.review.subprocess_timeout_seconds,
    )
    .await
    .context("spawn reviewer subprocess")?;

    match output.review {
        Some(r) if r.status == review::ReviewStatus::Pass => {
            client
                .transition_state(&issue.id, &issue.team_name, &cfg.review.pass_state)
                .await
                .context("transition to pass state")?;
            client
                .add_comment(&issue.id, &pass_comment_body(&r))
                .await
                .context("post pass comment")?;
            tracing::info!(issue = %issue.identifier, "reviewer PASS - moved to pass state");
        }
        Some(r) => {
            client
                .transition_state(&issue.id, &issue.team_name, &cfg.review.fail_state)
                .await
                .context("transition to fail state")?;
            client
                .add_comment(&issue.id, &fail_comment_body(&r))
                .await
                .context("post fail comment")?;
            tracing::warn!(issue = %issue.identifier, "reviewer FAIL - moved to fail state");
        }
        None => {
            // surface-don't-cancel: leave state alone, log a comment, move on.
            let reason = output
                .parse_error
                .unwrap_or_else(|| "reviewer produced no REVIEW.md".to_string());
            tracing::warn!(
                issue = %issue.identifier,
                exit = ?output.exit_code,
                reason = %reason,
                "reviewer produced no usable REVIEW.md - leaving state for operator"
            );
            let body = format!(
                "**anvil**: reviewer produced no usable `REVIEW.md`. Leaving state at `{}` for the operator to inspect.\n\n```\n{}\n```",
                cfg.review.state_name, reason
            );
            let _ = client.add_comment(&issue.id, &body).await;
        }
    }
    Ok(())
}

fn pass_comment_body(r: &review::Review) -> String {
    let mut out = String::from("**anvil reviewer: PASS**\n\n");
    if !r.findings.is_empty() {
        out.push_str("Advisory findings:\n");
        for f in &r.findings {
            out.push_str(&format!("- [{}] {}\n", f.grade.as_str(), f.finding));
        }
        out.push('\n');
    }
    if !r.notes.trim().is_empty() {
        out.push_str("Notes:\n");
        out.push_str(r.notes.trim());
        out.push('\n');
    }
    out
}

fn fail_comment_body(r: &review::Review) -> String {
    let mut out = String::from("**anvil reviewer: FAIL** - sending back for rework.\n\n");
    if !r.findings.is_empty() {
        out.push_str("Findings:\n");
        for f in &r.findings {
            out.push_str(&format!("- [{}] {}\n", f.grade.as_str(), f.finding));
        }
        out.push('\n');
    }
    if !r.notes.trim().is_empty() {
        out.push_str("Notes:\n");
        out.push_str(r.notes.trim());
        out.push('\n');
    }
    out
}

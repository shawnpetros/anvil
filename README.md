# anvil

Adversarial pre-PR reviewer that bolts onto OpenAI Symphony's `WORKFLOW.md` state machine. Anvil inserts one new Linear state, `Adversarial Review`, between Symphony's `In Progress` and `Human Review`. Symphony's builder finishes, transitions the issue to `Adversarial Review`, anvil polls, runs a cross-model audit on the diff, and either moves the issue forward to `Human Review` (pass) or back to `Rework` with findings posted as a comment (fail).

## How it relates to Symphony

Symphony orchestrates Codex agents through a documented Linear state machine: `Todo -> In Progress -> Human Review -> Merging -> Done` (with `Rework` as the bounce-back). Symphony is the elixir reference implementation at `https://github.com/openai/symphony`. Anvil does not modify Symphony's code. It runs as a separate Rust daemon, polls Linear independently, only acts on the new `Adversarial Review` state, and uses a different model from the one the builder used. The persona is a markdown file - swap models / prompts / providers without touching the binary.

## Setup

1. Install elixir + mise (Symphony's prerequisites) and clone Symphony:
   ```bash
   git clone https://github.com/openai/symphony ~/projects/symphony-upstream
   cd ~/projects/symphony-upstream/elixir
   mise trust && mise exec -- mix deps.get
   ```
2. Install anvil:
   ```bash
   git clone https://github.com/shawnpetros/anvil ~/projects/anvil
   cd ~/projects/anvil
   cargo install --path .
   ```
3. Copy the contents of `WORKFLOW.md.fragment.md` into your project's Symphony `WORKFLOW.md` (instructions in the fragment file).
4. Add the `Adversarial Review` workflow state to each Linear team anvil should review.
5. Set the `LINEAR_TOKEN` env var to your Linear personal API key. Anvil refuses to load if it sees a real token in the config file.
6. Write `~/.config/anvil/config.toml`:
   ```bash
   anvil init
   $EDITOR ~/.config/anvil/config.toml
   ```
7. Verify:
   ```bash
   anvil check
   ```
8. Run:
   ```bash
   anvil run
   ```

### Have Codex/Claude do this for you

Run: `claude -p "Bootstrap this repo for Anvil. Read README at https://github.com/shawnpetros/anvil and follow the setup."`

## Personas

Anvil ships with one persona at `templates/reviewer.md`. A persona is a markdown file with YAML frontmatter:

```yaml
---
name: anvil-reviewer
description: Adversarial code reviewer running pre-PR audit on Symphony-built diffs
agent_command: claude          # or "codex"
model_hint: sonnet             # opus | sonnet | haiku, optional
---
You are reviewing {{identifier}}: {{title}}.

The diff:
{{diff}}

[... your prompt body ...]
```

Available variables: `{{identifier}}`, `{{title}}`, `{{description}}`, `{{branch}}`, `{{workspace_path}}`, `{{diff}}`. Unknown placeholders are left in the rendered prompt verbatim so typos surface visibly.

The agent's job is to write `REVIEW.md` to the workspace and exit. Anvil owns every Linear write (state moves, comments, labels). The reviewer.md template documents the structured output contract.

New personas are just markdown files. Drop one in `templates/`, point `review.persona_path` at it. Same shape as `templates/reviewer.md`. Future personas (Penny meeting-attender, Argyle code-reviewer, anything else) follow the same pattern: frontmatter for the agent_command and model, body for the prompt.

## Architecture

```
loop {
    issues = linear.fetch_issues_in_state(teams, "Adversarial Review")
    for issue in issues {
        workspace = workspace_root / sanitize(issue.identifier)
        diff = git_diff_against_main(workspace, issue.branch_name)
        prompt = persona.render({ identifier, title, description, diff, branch, workspace_path })
        output = spawn_subprocess(persona.agent_command, prompt) // claude -p / codex exec
        review = parse(workspace / "REVIEW.md")
        match review.status {
            Pass => transition(issue, "Human Review");   add_comment(notes)
            Fail => transition(issue, "Rework");         add_comment(findings)
            None => leave_state_alone();                 add_comment("operator: REVIEW.md missing")
        }
    }
    sleep(poll_interval_seconds)
}
```

State transitions and comments are anvil's responsibility. The reviewer subprocess does not write to Linear. The Linear API token lives only in the env var named by `tracker.api_key_env`; the config file refuses to store a real one.

## Want the all-in-one?

If you'd rather not maintain Symphony plus Anvil as two separate processes, there's [Smithy](https://github.com/shawnpetros/smithy). A fork of Symphony with Anvil's adversarial-review state baked in, plus dual agent runtimes (Codex + Claude Code), Linear OAuth identity, label-gated autonomous merge, model-summarized run logs, and a max-retry circuit breaker. One process, all the opinions.

Anvil stays as the option for users who want vanilla Symphony plus adversarial review without forking.

## License

MIT.

## Author

Shawn Petros (`shawn.petros@gmail.com`).

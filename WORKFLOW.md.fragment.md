# anvil: WORKFLOW.md fragment

Drop this into your project's Symphony `WORKFLOW.md`. Anvil bolts onto Symphony by inserting one new state, `Adversarial Review`, between `In Progress` and `Human Review`. Symphony's agent transitions to `Adversarial Review` instead of going straight to `Human Review`; anvil polls for that state, runs a cross-model audit, and either passes the issue forward or kicks it back to `Rework`.

## 1. Add the new state to `tracker.active_states`

In the YAML frontmatter at the top of `WORKFLOW.md`:

```yaml
tracker:
  kind: linear
  project_slug: "symphony-0c79b11b75ea"
  active_states:
    - Todo
    - In Progress
    - Adversarial Review   # <-- add this
    - Merging
    - Rework
```

Symphony's poll loop already ignores states that don't appear in `active_states`, so anvil's read-only access works whether or not you add it here. Adding it just keeps Symphony's view consistent (e.g. its dashboard counts) when the agent itself isn't acting on the state.

## 2. Add `Adversarial Review` to the `Status map` section

In the markdown body, under `## Status map`:

```markdown
- `Adversarial Review` -> anvil-owned audit state. The orchestrator must NOT
  run agents on issues here; anvil polls independently and either transitions
  to `Human Review` (pass) or `Rework` (fail with findings comment).
```

## 3. Update the agent's "completion" instruction

In the `## Step 2: Execution phase` section, the existing flow says:

> 12. Only then move issue to `Human Review`.

Change that to:

> 12. Only then move issue to `Adversarial Review`. Anvil will run the
>     adversarial audit and transition to `Human Review` on pass or `Rework`
>     on fail. Do NOT transition the issue to `Human Review` directly.

If you have an explicit transition mutation in your skill files (e.g. `update_issue(state="Human Review")`), change the target to `Adversarial Review`.

## 4. Linear setup

In Linear, add the `Adversarial Review` workflow state to each team whose issues anvil should review. Place it between `In Progress` and `Human Review` so the activity feed shows the audit step in order. Anvil resolves state names per team, so the spelling must match exactly (case-insensitive).

That's it. Anvil runs as a separate process; nothing in the Symphony elixir code changes.

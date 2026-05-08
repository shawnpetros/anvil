#!/usr/bin/env bash
# scripts/demo-preflight.sh
#
# Anvil demo-recording preflight. Verifies the environment, creates a demo
# Linear ticket in `In Progress`, prints the recording playbook with the
# specific URL to drag.
#
# Idempotent: re-running uses the existing demo ticket if one already exists
# (any ticket whose title starts with "DEMO:" in the configured team).
#
# Requires: anvil binary on PATH, ~/.config/anvil/config.toml, Linear API
# token in the env var named by `tracker.api_key_env` in that config
# (defaults to LINEAR_TOKEN).

set -euo pipefail

CONFIG_PATH="${ANVIL_CONFIG:-$HOME/.config/anvil/config.toml}"
DEMO_TITLE_PREFIX="DEMO:"

color_ok()   { printf "\033[32m[OK]\033[0m   %s\n" "$*"; }
color_warn() { printf "\033[33m[WARN]\033[0m %s\n" "$*"; }
color_fail() { printf "\033[31m[FAIL]\033[0m %s\n" "$*" >&2; }

# 1. anvil binary
if ! command -v anvil >/dev/null 2>&1; then
  color_fail "anvil binary not on PATH"
  echo "        install: cd ~/projects/anvil && cargo install --path ." >&2
  exit 1
fi
color_ok "anvil binary at $(command -v anvil)"

# 2. config file
if [ ! -f "$CONFIG_PATH" ]; then
  color_fail "config not found at $CONFIG_PATH"
  echo "        run: anvil init" >&2
  exit 1
fi
color_ok "config at $CONFIG_PATH"

# 3. anvil check
if ! anvil check >/dev/null 2>&1; then
  color_fail "'anvil check' failed; run it directly to see the errors"
  exit 1
fi
color_ok "anvil check passed"

# 4. Linear token env var
TOKEN_VAR=$(awk -F'=' '
  /^[[:space:]]*api_key_env[[:space:]]*=/ {
    gsub(/[[:space:]"]/, "", $2); print $2; exit
  }
' "$CONFIG_PATH")
TOKEN_VAR="${TOKEN_VAR:-LINEAR_TOKEN}"
TOKEN_VALUE="${!TOKEN_VAR:-}"
if [ -z "$TOKEN_VALUE" ]; then
  color_fail "$TOKEN_VAR is not set in env"
  exit 1
fi
color_ok "$TOKEN_VAR is set"

# 5. team name from config
TEAM_NAME=$(awk -F'=' '
  /^[[:space:]]*team[[:space:]]*=/ {
    gsub(/[[:space:]"]/, "", $2); print $2; exit
  }
' "$CONFIG_PATH")
if [ -z "$TEAM_NAME" ]; then
  color_fail "could not parse team name from $CONFIG_PATH"
  exit 1
fi
color_ok "team in config: $TEAM_NAME"

# 6. resolve team id + workflow states via Linear API
RESOLVE_RESP=$(curl -fsS -X POST https://api.linear.app/graphql \
  -H "Authorization: $TOKEN_VALUE" \
  -H "Content-Type: application/json" \
  -d '{"query":"{ teams { nodes { id name key states { nodes { id name type } } } } }"}')

eval "$(printf '%s' "$RESOLVE_RESP" | python3 - "$TEAM_NAME" <<'PY'
import json, sys
data = json.load(sys.stdin)
target = sys.argv[1]
team_id = ""
ip_id   = ""
adv_id  = ""
for t in (data.get("data", {}).get("teams", {}) or {}).get("nodes", []) or []:
    if t.get("name") == target or t.get("key") == target:
        team_id = t["id"]
        for s in (t.get("states") or {}).get("nodes", []) or []:
            if s.get("name") == "In Progress": ip_id = s["id"]
            if s.get("name") == "Adversarial Review": adv_id = s["id"]
        break
print(f'TEAM_ID="{team_id}"')
print(f'IN_PROGRESS_STATE_ID="{ip_id}"')
print(f'ADV_STATE_ID="{adv_id}"')
PY
)"

if [ -z "${TEAM_ID:-}" ]; then
  color_fail "team '$TEAM_NAME' not found in Linear"
  exit 1
fi
color_ok "team id: $TEAM_ID"

if [ -z "${IN_PROGRESS_STATE_ID:-}" ]; then
  color_fail "team is missing 'In Progress' workflow state"
  exit 1
fi
color_ok "'In Progress' state present"

if [ -z "${ADV_STATE_ID:-}" ]; then
  color_fail "team is missing 'Adversarial Review' workflow state"
  echo "        add it in Linear settings, between 'In Progress' and 'Human Review'" >&2
  exit 1
fi
color_ok "'Adversarial Review' state present"

# 7. existing demo ticket?
LIST_BODY=$(python3 -c "
import json, sys
print(json.dumps({
  'query': 'query(\$tid: String!) { issues(filter: { team: { id: { eq: \$tid } }, title: { startsWith: \"$DEMO_TITLE_PREFIX\" } }) { nodes { id identifier title url branchName } } }',
  'variables': {'tid': '$TEAM_ID'}
}))
")
LIST_RESP=$(curl -fsS -X POST https://api.linear.app/graphql \
  -H "Authorization: $TOKEN_VALUE" \
  -H "Content-Type: application/json" \
  -d "$LIST_BODY")

eval "$(printf '%s' "$LIST_RESP" | python3 <<'PY'
import json, sys
data = json.load(sys.stdin)
nodes = (data.get("data", {}).get("issues", {}) or {}).get("nodes", []) or []
if nodes:
    n = nodes[0]
    print(f'EXISTING_IDENT="{n.get("identifier","")}"')
    print(f'EXISTING_URL="{n.get("url","")}"')
    print(f'EXISTING_BRANCH="{n.get("branchName","")}"')
else:
    print('EXISTING_IDENT=""')
    print('EXISTING_URL=""')
    print('EXISTING_BRANCH=""')
PY
)"

if [ -n "${EXISTING_IDENT:-}" ]; then
  color_ok "reusing existing demo ticket $EXISTING_IDENT"
  TICKET_URL="$EXISTING_URL"
  TICKET_IDENT="$EXISTING_IDENT"
  TICKET_BRANCH="$EXISTING_BRANCH"
else
  # 8. create new demo ticket
  CREATE_BODY=$(python3 -c "
import json
print(json.dumps({
  'query': 'mutation(\$input: IssueCreateInput!) { issueCreate(input: \$input) { success issue { id identifier url branchName } } }',
  'variables': {'input': {
    'teamId': '$TEAM_ID',
    'stateId': '$IN_PROGRESS_STATE_ID',
    'title': 'DEMO: Add cache invalidation hook',
    'description': '''Demo ticket for recording the Anvil adversarial-review GIF.

When recording starts, drag this ticket from \"In Progress\" to \"Adversarial Review\". Anvil will pick it up on the next poll cycle, run the cross-model review on the diff, and transition to \"Human Review\" (PASS) or \"Rework\" (FAIL).

The branch name Linear has assigned for this ticket is shown in the script output. For the demo to produce a real review, that branch must exist in your target repo with a real diff against main. Smaller diffs (60-300 LOC) review faster and make better demos.'''
  }}
}))
")
  CREATE_RESP=$(curl -fsS -X POST https://api.linear.app/graphql \
    -H "Authorization: $TOKEN_VALUE" \
    -H "Content-Type: application/json" \
    -d "$CREATE_BODY")

  eval "$(printf '%s' "$CREATE_RESP" | python3 <<'PY'
import json, sys
data = json.load(sys.stdin)
issue = (data.get("data", {}).get("issueCreate", {}) or {}).get("issue", {}) or {}
print(f'TICKET_IDENT="{issue.get("identifier","")}"')
print(f'TICKET_URL="{issue.get("url","")}"')
print(f'TICKET_BRANCH="{issue.get("branchName","")}"')
PY
)"

  if [ -z "${TICKET_IDENT:-}" ]; then
    color_fail "demo ticket creation failed"
    echo "        response: $CREATE_RESP" >&2
    exit 1
  fi
  color_ok "demo ticket created: $TICKET_IDENT"
fi

# 9. recording-tool detection
echo
echo "=== Recording tools ==="
if [ -d "/Applications/Kap.app" ]; then
  color_ok "Kap installed (record + export GIF in one step)"
elif command -v gifski >/dev/null 2>&1; then
  color_ok "gifski installed (use with Cmd+Shift+5 to record, then convert)"
elif command -v ffmpeg >/dev/null 2>&1; then
  color_ok "ffmpeg installed (low-level conversion)"
else
  color_warn "no recording tool found"
  echo "        install one of:"
  echo "          brew install --cask kap"
  echo "          brew install --cask gifski"
  echo "          brew install ffmpeg"
fi

# 10. recording playbook
cat <<EOF

================================================================
ENVIRONMENT READY. Recording playbook:

ticket: $TICKET_IDENT
url:    $TICKET_URL
branch: $TICKET_BRANCH

BEFORE you press record:
  Make sure the branch '$TICKET_BRANCH' exists in your target repo
  (the repo configured in $CONFIG_PATH under repos.* / workspaces).
  It needs a real diff against main for the reviewer to read.

DURING recording (~45 seconds):
  1. Split the screen: Linear browser left, terminal right. Bump
     terminal font to 18+ for legibility.
  2. Press Cmd+Shift+5 (or open Kap), set the recording area to
     cover both windows, hit record.
  3. In terminal:    anvil run
  4. Wait 2-3 seconds for the first poll cycle.
  5. In Linear, open the ticket above and drag it from
     'In Progress' to 'Adversarial Review'.
  6. Watch the terminal: anvil detects the ticket within ~10s,
     spawns the reviewer, parses the verdict.
  7. Switch to Linear: state moves to 'Human Review' (or 'Rework'),
     anvil's review comment appears.
  8. Stop the recording.

AFTER recording:
  Convert to GIF:
    gifski --fps 12 --width 900 -o assets/demo.gif input.mp4
  OR:
    ffmpeg -i input.mp4 -vf "fps=12,scale=900:-1:flags=lanczos" \\
           -loop 0 assets/demo.gif

  Embed in README:
    <p align="center">
      <img src="assets/demo.gif" alt="Anvil in action" width="900" />
    </p>

CLEANUP (optional):
  When you're done with this preflight ticket, mark it Done or
  delete it. Re-running this script will reuse it if it's still
  open with the DEMO: prefix.
================================================================
EOF

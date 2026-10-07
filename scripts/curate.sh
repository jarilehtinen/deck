#!/bin/bash
# Curate run: Claude builds the Curated list following curate/prompt.md (see README).
# launchd runs this on Mondays (io.github.jarilehtinen.deck-curate.plist), and running
# it by hand does the same right away. Log: ~/.cache/deck/curate.log. On failure the
# previous list stays in effect.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
PROMPT="$REPO/curate/prompt.md"
LOG="$HOME/.cache/deck/curate.log"
# Claude's working directory: candidate files are written here and stay until the
# next run. `deck curate` gets only files from this directory on stdin.
WORK="$HOME/.cache/deck/curate"
CURATED="$HOME/.config/deck/curated.json"
# Claude Code saves long tool output (deck taste) in this working directory's
# project folder ~/.claude/projects/<slug>/, where slug is the path with everything
# but letters and digits replaced by hyphens. Read may read only from there, not
# other projects' conversations.
SLUG="$(printf %s "$WORK" | sed 's/[^A-Za-z0-9]/-/g')"

# install-curate.sh writes a PATH for the launchd job that contains deck, claude and jq.
# As a fallback (running by hand with a narrow PATH), the usual install dirs are added.
export PATH="$PATH:$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

mkdir -p "$WORK" "$(dirname "$LOG")"

log() {
    printf '%s  %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" | tee -a "$LOG"
}

missing=0
for cmd in deck claude jq; do
    if ! command -v "$cmd" > /dev/null; then
        log "error: $cmd not found (PATH=$PATH)"
        missing=1
    fi
done
[ "$missing" -eq 0 ] || exit 1

# Turns claude -p's stream-json events into readable log lines. Other lines
# (e.g. claude's error messages) go to the log as they are.
FORMAT='
def text: if type == "string" then . else map(.text? // "") | join("") end;
def report: (capture("(?s)(?<j>\\{.*\\})").j | fromjson?) // null;
(fromjson? // {type: "raw", line: .})
| if .type == "raw" then .line
elif .type == "system" and .subtype == "init" then
  "start: model \(.model)"
elif .type == "assistant" then
  .message.content[]
  | if .type == "text" then "claude: \(.text)"
    elif .type == "tool_use" then
      if .name == "Bash" then "$ \(.input.command)"
      elif .name == "WebSearch" then "search: \(.input.query)"
      elif .name == "Read" then "read: \(.input.file_path)"
      elif .name == "ToolSearch" then empty
      elif .name == "Write" then
        "write: \(.input.file_path) (\((.input.content | fromjson? | length) // "?") candidates)"
      else "\(.name): \(.input | tostring | .[0:200])" end
    else empty end
elif .type == "user" then
  .message.content[]? | select(.type == "tool_result")
  | (.content | text) as $out
  | if .is_error then "  ! \($out | .[0:300])"
    elif ($out | report | type) == "object" and ($out | report | has("missing")) then
      ($out | report) as $r
      | "  accepted \($r.accepted | length), unused \($r.unused // [] | length), missing \($r.missing), searched \($r.searched // "?"), searches left \($r.searches_left // "?")"
        + ([$r.rejected[] | "\n    rejected \(.reason): \(.artist) – \(.album)"] | join(""))
    else "  ok (\($out | length) chars)" end
elif .type == "result" then
  "end: \(.subtype), \(.num_turns) turns, \(.duration_ms / 1000 | floor) s, $\(.total_cost_usd * 100 | round / 100)"
else empty end
'

log "=== curate: start ($REPO)"

if [ ! -f "$PROMPT" ]; then
    log "error: $PROMPT not found"
    exit 1
fi

# Remove the previous run's candidate files, so that Write creates them anew.
rm -f "$WORK"/*.json
touch "$WORK/.start"
cd "$WORK" || exit 1

claude -p "$(cat "$PROMPT")" \
    --permission-mode dontAsk \
    --allowedTools "Bash(deck taste)" "Bash(deck curate:*)" "Write(./**)" \
    "Read(./**)" "Read(~/.claude/projects/$SLUG/**)" "WebSearch" \
    --strict-mcp-config \
    --no-session-persistence \
    --output-format stream-json --verbose \
    < /dev/null 2>&1 \
    | jq --unbuffered -R -r "$FORMAT" \
    | while IFS= read -r line; do log "$line"; done
status=${PIPESTATUS[0]}

if [ "$CURATED" -nt "$WORK/.start" ]; then
    log "=== curate: done, $(jq -r '"\(.albums | length) albums, round \(.round)"' "$CURATED")"
    exit 0
fi
log "=== curate: failed (claude exit $status), $CURATED not updated, previous list stays"
exit 1

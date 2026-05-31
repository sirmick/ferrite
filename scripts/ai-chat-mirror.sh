#!/usr/bin/env bash
# Mirror this Claude Code session's conversation into Ferrite's browser
# AI panel, via `ferrite-ctl chat-post` -> POST /api/ai/chat-inject ->
# the /ws/chat fan-out. Invoked by the hooks wired up in
# .claude/settings.json; reads the hook event JSON on stdin.
#
#   ai-chat-mirror.sh user    # UserPromptSubmit: {prompt}
#   ai-chat-mirror.sh tool    # PostToolUse:      {tool_name, tool_input, tool_response}
#   ai-chat-mirror.sh stop    # Stop:             {transcript_path}
#
# Best-effort glue: never fail a hook (always exit 0), and stay quiet
# when ferrited / jq / the binary aren't around (e.g. CI).
set -u

CTL="${FERRITE_CTL:-/home/mick/ferrite/target/release/ferrite-ctl}"
CONNECT="${FERRITE_CTL_CONNECT:-http://127.0.0.1:10001}"
EVENT="${1:-}"

command -v jq >/dev/null 2>&1 || exit 0
[ -x "$CTL" ] || exit 0

post() { "$CTL" --connect "$CONNECT" chat-post "$@" >/dev/null 2>&1 || true; }

payload="$(cat)"

case "$EVENT" in
  user)
    text="$(printf '%s' "$payload" | jq -r '.prompt // empty')"
    [ -n "$text" ] && post user "$text"
    ;;
  tool)
    name="$(printf '%s' "$payload" | jq -r '.tool_name // empty')"
    [ -z "$name" ] && exit 0
    input="$(printf '%s' "$payload" | jq -c '.tool_input // {}' 2>/dev/null | head -c 2000)"
    result="$(printf '%s' "$payload" | jq -r '
        (.tool_response // empty)
        | if type=="string" then .
          elif type=="object" then (.stdout // .output // .content // (.|tostring))
          else tostring end
      ' 2>/dev/null | head -c 4000)"
    post tool "" --tool-name "$name" --tool-input "$input" --tool-result "$result"
    ;;
  stop)
    tp="$(printf '%s' "$payload" | jq -r '.transcript_path // empty')"
    [ -f "$tp" ] || exit 0
    # Walk the JSONL transcript backwards to the most recent assistant
    # message and emit its joined text blocks.
    text="$(tac "$tp" | while IFS= read -r line; do
        printf '%s' "$line" | jq -e 'select(.type=="assistant")' >/dev/null 2>&1 || continue
        printf '%s' "$line" | jq -r '[.message.content[]? | select(.type=="text") | .text] | join("\n")'
        break
      done)"
    [ -n "$text" ] && post assistant "$text"
    ;;
  *)
    : ;;
esac
exit 0

#!/usr/bin/env bash
# Mirror this Claude Code session's conversation into Ferrite's AI panel:
# `ferrite-ctl chat-post` publishes onto the AI session bus
# (ferrite/ingest/ai -> the `ai` store kind), which every panel renders,
# grouped by this session's id. Invoked by the hooks wired up in
# .claude/settings.json; reads the hook event JSON on stdin.
#
#   ai-chat-mirror.sh user    # UserPromptSubmit: {prompt}
#   ai-chat-mirror.sh tool    # PostToolUse:      {tool_name, tool_input, tool_response}
#   ai-chat-mirror.sh stop    # Stop:             {transcript_path}
#
# Opt-in: it only runs with FERRITE_AI_MIRROR=1 in Claude Code's
# environment (e.g. `"env": {"FERRITE_AI_MIRROR": "1"}` in
# .claude/settings.local.json). Off by default, so ordinary coding
# sessions in this repo don't write into the radio AI's transcript.
#
# Harness messages that arrive as "user prompts" (background-task
# notifications, slash-command echoes) are not mirrored, and
# <system-reminder> blocks are stripped from real ones.
#
# Best-effort glue: never fail a hook (always exit 0), and stay quiet
# when ferrited / jq / the binary aren't around (e.g. CI).
set -u

[ "${FERRITE_AI_MIRROR:-0}" = 1 ] || exit 0

# Resolve the binary relative to this script (the repo may live
# anywhere) and dial the daemon on run.sh's default port. Both were
# hard-coded to another checkout + the old port, so every hook exited
# quietly and nothing ever reached the AI panel.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CTL="${FERRITE_CTL:-$HERE/../target/release/ferrite-ctl}"
CONNECT="${FERRITE_CTL_CONNECT:-http://127.0.0.1:${FERRITED_PORT:-12001}}"
EVENT="${1:-}"

command -v jq >/dev/null 2>&1 || exit 0
[ -x "$CTL" ] || exit 0

payload="$(cat)"

# Group this Claude Code session's turns on the panel by its session id.
session="$(printf '%s' "$payload" | jq -r '.session_id // empty' 2>/dev/null)"
post() { "$CTL" --connect "$CONNECT" chat-post "$@" ${session:+--session "$session"} >/dev/null 2>&1 || true; }

case "$EVENT" in
  user)
    # Drop <system-reminder> blocks; skip prompts that are only harness
    # envelopes (<task-notification>, <local-command-…>, <command-…>).
    text="$(printf '%s' "$payload" | jq -r '
        (.prompt // "")
        | gsub("(?s)<system-reminder>.*?</system-reminder>"; "")
        | gsub("^\\s+|\\s+$"; "")')"
    case "$text" in
      ""|"<task-notification>"*|"<local-command-"*|"<command-name>"*|"<command-message>"*) exit 0 ;;
    esac
    post user "$text"
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

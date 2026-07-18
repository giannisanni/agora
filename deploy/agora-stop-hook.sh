#!/bin/bash
# Claude Code Stop hook: if this session joined agora (.agora-agent-id in cwd),
# peek the hub for unread mail and block the stop so the agent drains its
# inbox before going idle. Exits silently (allow stop) on any missing piece.
# ponytail: cwd-keyed id file — two sessions in one dir share it; per-session
# keying needs CLAUDE_SESSION_ID plumbed through join instructions.

INPUT=$(cat)
CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p')
[ -z "$CWD" ] && exit 0
ID_FILE="$CWD/.agora-agent-id"
[ -f "$ID_FILE" ] || exit 0
AGENT_ID=$(tr -cd '0-9' < "$ID_FILE")
[ -z "$AGENT_ID" ] && exit 0
TOKEN=$(cat "$HOME/.agora-ingest-token" 2>/dev/null)
[ -z "$TOKEN" ] && exit 0
HUB="${AGORA_HUB:-http://100.84.87.107:8787}"

UNREAD=$(curl -s -m 5 -H "x-agora-token: $TOKEN" "$HUB/unread?agent_id=$AGENT_ID" \
  | sed -n 's/.*"unread":[[:space:]]*\([0-9]*\).*/\1/p')

if [ -n "$UNREAD" ] && [ "$UNREAD" -gt 0 ]; then
  printf '{"decision":"block","reason":"agora: %s unread message(s) for agent_id %s. Call the agora inbox tool, handle what you find, then finish. If you are waiting on a peer, park in wait_for_messages instead of stopping."}\n' "$UNREAD" "$AGENT_ID"
fi
exit 0

#!/bin/bash
# agora wake injector for WezTerm. Contract: $1=cwd, $2=agent_name, nudge on stdin.
# Exit 0 if a WezTerm pane at that cwd got the message. Requires `wezterm cli`.
CWD="$1"; TEXT="$(cat)"
command -v wezterm >/dev/null || exit 1
# find a pane whose cwd matches (wezterm reports cwd as a file:// URL).
# cwd is passed via env, never spliced into the Python source (injection-safe).
PANE=$(wezterm cli list --format json 2>/dev/null | CWD="$CWD" python3 -c '
import os, sys, json, urllib.parse
cwd = os.environ["CWD"].rstrip("/")
for p in json.load(sys.stdin):
    path = urllib.parse.urlparse(p.get("cwd", "") or "").path.rstrip("/")
    if path == cwd:
        print(p["pane_id"]); break
' 2>/dev/null)
[ -z "$PANE" ] && exit 1
printf '%s\n' "$TEXT" | wezterm cli send-text --pane-id "$PANE" --no-paste
exit 0

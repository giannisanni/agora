#!/bin/bash
# agora wake injector for Kitty. $1=cwd, $2=agent_name, nudge on stdin.
# Requires kitty remote control enabled (allow_remote_control yes).
CWD="$1"; TEXT="$(cat)"
command -v kitty >/dev/null || exit 1
# cwd is passed via env, never spliced into the Python source (injection-safe).
MATCH=$(kitty @ ls 2>/dev/null | CWD="$CWD" python3 -c '
import os, sys, json
cwd = os.environ["CWD"].rstrip("/")
for w in json.load(sys.stdin):
    for t in w.get("tabs", []):
        for win in t.get("windows", []):
            if (win.get("cwd", "") or "").rstrip("/") == cwd:
                print(win["id"]); sys.exit(0)
' 2>/dev/null)
[ -z "$MATCH" ] && exit 1
kitty @ send-text --match "id:$MATCH" "$TEXT
"
exit 0

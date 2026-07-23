#!/bin/bash
# agora wake injector for Kitty. $1=cwd, $2=agent_name, nudge on stdin.
# Requires kitty remote control enabled (allow_remote_control yes).
CWD="$1"; TEXT="$(cat)"
command -v kitty >/dev/null || exit 1
MATCH=$(kitty @ ls 2>/dev/null | python3 -c "import sys,json
d=json.load(sys.stdin)
for w in d:
  for t in w.get('tabs',[]):
    for win in t.get('windows',[]):
      if (win.get('cwd','') or '').rstrip('/')=='$CWD'.rstrip('/'):
        print(win['id']); sys.exit(0)" 2>/dev/null)
[ -z "$MATCH" ] && exit 1
kitty @ send-text --match "id:$MATCH" "$TEXT
"
exit 0

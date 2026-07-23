#!/bin/bash
# agora wake injector for WezTerm. Contract: $1=cwd, $2=agent_name, nudge on stdin.
# Exit 0 if a WezTerm pane at that cwd got the message. Requires `wezterm cli`.
CWD="$1"; TEXT="$(cat)"
command -v wezterm >/dev/null || exit 1
# find a pane whose cwd matches (wezterm reports cwd as a file:// URL)
PANE=$(wezterm cli list --format json 2>/dev/null \
  | python3 -c "import sys,json,urllib.parse;
d=json.load(sys.stdin)
for p in d:
    u=p.get('cwd','') or ''
    path=urllib.parse.urlparse(u).path
    if path.rstrip('/')=='$CWD'.rstrip('/'):
        print(p['pane_id']); break" 2>/dev/null)
[ -z "$PANE" ] && exit 1
printf '%s\n' "$TEXT" | wezterm cli send-text --pane-id "$PANE" --no-paste
exit 0

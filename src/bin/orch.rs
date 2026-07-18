//! agora orchestrator: spawn/kill/restart/list agent fleets in tmux sessions,
//! locally or on a remote host over ssh.
//!
//!   orch spawn <name> [--harness claude|codex] [--room dev] [--on user@host]
//!   orch kill <name> [--on user@host]
//!   orch restart <name> [--on user@host]
//!   orch agents [--on user@host]
//!
//! Each agent gets its own dir ~/agora-agents/<name> (cwd => own
//! .agora-agent-id => wake shim + Stop hook work), a tmux session
//! agora-<name>, and a join prompt that ends in a resident
//! wait_for_messages loop. Spawn args are saved there so restart replays them.

use std::process::Command;

/// Single-quote a string for safe embedding in a shell command.
fn sh_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn run(host: Option<&str>, script: &str) -> (bool, String) {
    let out = match host {
        // BatchMode: never hang on an interactive auth prompt (e.g. Tailscale
        // SSH re-auth) — fail fast with an error the caller can show.
        Some(h) => Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", h, script])
            .output(),
        None => Command::new("bash").args(["-c", script]).output(),
    };
    match out {
        Ok(o) => (
            o.status.success(),
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn join_prompt(name: &str, room: &str, harness: &str) -> String {
    format!(
        "Join agora: call join_room with room=\"{room}\", name=\"{name}\", and \
         harness=\"{harness}\" (always pass the harness field). Write your \
         agent_id to .agora-agent-id in your cwd. Set your status with \
         set_status. Check your inbox and handle anything there. Then loop \
         forever: park in wait_for_messages WITHOUT passing timeout_secs (so it \
         uses your operator-configured park timeout), handle whatever arrives \
         (reply, do tasks, update status), and park again. You are a resident \
         agent; do not end your turn."
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let mut name = String::new();
    let mut harness = "claude".to_string();
    let mut room = "dev".to_string();
    let mut host: Option<String> = None;
    let mut model: Option<String> = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--harness" => harness = it.next().cloned().unwrap_or(harness),
            "--room" => room = it.next().cloned().unwrap_or(room),
            "--on" => host = it.next().cloned(),
            "--model" => model = it.next().cloned(),
            other if name.is_empty() => name = other.to_string(),
            _ => {}
        }
    }
    let host = host.as_deref();

    match cmd {
        "spawn" => {
            if name.is_empty() {
                eprintln!("usage: orch spawn <name> [--harness claude|codex] [--room r] [--on user@host]");
                std::process::exit(1);
            }
            // per-harness launch command; --model appended only when given, so
            // each harness otherwise uses its own configured default model.
            let m = model.as_deref().map(|m| format!(" --model {m}")).unwrap_or_default();
            let m_codex = model.as_deref().map(|m| format!(" -m {m}")).unwrap_or_default();
            // Non-interactive launch so first-run trust/permission prompts don't
            // block a headless (esp. remote) spawn. Default: acceptEdits — the
            // agent proceeds without prompting for edits but still refuses/asks
            // on higher-risk actions. Full sandbox/permission bypass is OFF by
            // default and only enabled with AGORA_YOLO=1, an explicit operator
            // opt-in (spawned agents run with the host user's credentials, so a
            // blanket bypass = arbitrary host command execution — see README).
            let yolo = std::env::var("AGORA_YOLO").is_ok_and(|v| v == "1");
            let bin = match harness.as_str() {
                "claude" | "claude-code" => {
                    // Scoped permission (not a blanket bypass): allowlist ONLY the
                    // agora MCP tools a resident agent needs, so its join/inbox/post
                    // calls run unattended while everything else still prompts.
                    // AGORA_YOLO=1 upgrades to full skip for do-anything workers.
                    let p = if yolo {
                        "--dangerously-skip-permissions".to_string()
                    } else {
                        "--permission-mode acceptEdits --allowedTools \"mcp__agora__join_room\" \"mcp__agora__inbox\" \"mcp__agora__post\" \"mcp__agora__peers\" \"mcp__agora__set_status\" \"mcp__agora__wait_for_messages\" \"mcp__agora__feed\" \"Write\"".to_string()
                    };
                    // strict-mcp-config: load ONLY agora-mcp.json (written below),
                    // ignoring project/global MCP configs.
                    format!("claude {p} --strict-mcp-config --mcp-config agora-mcp.json{m}")
                }
                "codex" => {
                    let p = if yolo { "--dangerously-bypass-approvals-and-sandbox" } else { "--full-auto" };
                    format!("codex {p}{m_codex}")
                }
                "opencode" => format!("opencode run{m}"),
                other => {
                    // Unknown harness — refuse rather than trying to exec it as a
                    // command (a bare model like "sonnet-5" landing here would loop
                    // forever as "command not found"). Guide toward correct syntax.
                    eprintln!(
                        "unknown harness '{other}'. Use claude | codex | opencode. \
                         For a model, use model:<id> (e.g. /spawn {name} claude model:{other})."
                    );
                    std::process::exit(2);
                }
            };
            let prompt = join_prompt(&name, &room, &harness);
            // self-healing wrapper: if the agent process exits (turn ends /
            // crashes), restart it after a short backoff — keeps the agent
            // resident and the tmux session (and server) alive. `.agora-stop`
            // breaks the loop for a clean kill.
            // respawn loop with exponential backoff: a fast-exiting agent (crash,
            // usage limit) backs off 5→10→...→300s instead of hammering; a run
            // that lasted >60s resets the backoff.
            // NOTE: prompt is passed via the AGORA_PROMPT env var, not as a bare
            // positional — claude's --mcp-config is greedy (takes multiple file
            // args) and would otherwise swallow the prompt as a config filename.
            // The `--` terminates option parsing so the prompt is unambiguously
            // the query.
            let run_sh = format!(
                "#!/bin/bash\ncd \"$(dirname \"$0\")\"\nb=5\nwhile [ ! -f .agora-stop ]; do\n  s=$SECONDS\n  {bin} -- \"$(cat prompt.txt)\"\n  [ -f .agora-stop ] && break\n  if [ $((SECONDS - s)) -ge 60 ]; then b=5; else b=$((b*2)); [ $b -gt 300 ] && b=300; fi\n  echo \"[agora] agent exited; restarting in ${{b}}s\"\n  sleep $b\ndone\n"
            );
            // Pre-trust the agent dir in the harness config so a headless/remote
            // spawn doesn't stall on the one-time folder-trust prompt. This only
            // marks a directory WE just created as trusted — it doesn't change
            // permission behavior (that's the acceptEdits/AGORA_YOLO gate above).
            let pretrust = match harness.as_str() {
                // Pre-decide every project MCP server (from ~/.mcp.json etc.) by
                // marking them all disabled for the agent dir, so claude never
                // shows the "N new MCP servers found, approve?" prompt. We also
                // accept trust + mark onboarding complete. Server names are read
                // live from ~/.mcp.json so this stays correct as it changes.
                "claude" | "claude-code" =>
                    "AGDIR=\"$HOME/agora-agents/PLACEHOLDER\" python3 -c 'import json,os; p=os.path.expanduser(\"~/.claude.json\"); d=json.load(open(p)) if os.path.exists(p) else {}; mp=os.path.expanduser(\"~/.mcp.json\"); names=list(json.load(open(mp)).get(\"mcpServers\",{}).keys()) if os.path.exists(mp) else []; e=d.setdefault(\"projects\",{}).setdefault(os.environ[\"AGDIR\"],{}); e[\"hasTrustDialogAccepted\"]=True; e[\"hasCompletedProjectOnboarding\"]=True; e[\"projectOnboardingSeenCount\"]=9; e[\"enabledMcpjsonServers\"]=[]; e[\"disabledMcpjsonServers\"]=names; e[\"enableAllProjectMcpServers\"]=False; json.dump(d,open(p,\"w\"))' 2>/dev/null; ".to_string(),
                "codex" =>
                    "AGDIR=\"$HOME/agora-agents/PLACEHOLDER\"; mkdir -p ~/.codex; grep -qF \"[projects.\\\"$AGDIR\\\"]\" ~/.codex/config.toml 2>/dev/null || printf '\\n[projects.\"%s\"]\\ntrust_level = \"trusted\"\\n' \"$AGDIR\" >> ~/.codex/config.toml; ".to_string(),
                _ => String::new(),
            }.replace("PLACEHOLDER", &name);
            // minimal MCP config: just the agora hub, for claude --strict-mcp-config
            let hub_url = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
            let mcp_json = format!("{{\"mcpServers\":{{\"agora\":{{\"type\":\"http\",\"url\":\"{hub_url}/mcp\"}}}}}}");
            let script = format!(
                "mkdir -p ~/agora-agents/{name} && cd ~/agora-agents/{name} && \
                 printf '%s %s\\n' '{harness}' '{room}' > .agora-spawn && \
                 rm -f .agora-stop && \
                 printf '%s' {mcp_q} > agora-mcp.json && \
                 {pretrust}\
                 printf '%s' {prompt_q} > prompt.txt && \
                 printf '%s' {run_q} > run.sh && \
                 tmux new-session -d -s agora-{name} 'bash run.sh' && \
                 tmux set-option -g exit-empty off 2>/dev/null; \
                 tmux set-option -g exit-unattached off 2>/dev/null; \
                 echo spawned agora-{name}",
                prompt_q = sh_squote(&prompt),
                run_q = sh_squote(&run_sh),
                mcp_q = sh_squote(&mcp_json),
            );
            let (ok, out) = run(host, &script);
            print!("{out}");
            std::process::exit(if ok { 0 } else { 1 });
        }
        "kill" => {
            // Atomic stop: stop-flag (no relaunch) → kill session → drop id-file
            // → mark the agent stale on the hub, all in one sequential script so
            // a still-parked agent can't re-ping green in a race. Room comes from
            // the spawn record; token/hub from the local env + token file.
            let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
            let script = format!(
                "cd ~/agora-agents/{name} 2>/dev/null || exit 0; \
                 touch .agora-stop; \
                 tmux kill-session -t agora-{name} 2>/dev/null; \
                 rm -f .agora-agent-id; \
                 R=$(awk '{{print $2}}' .agora-spawn 2>/dev/null); \
                 TOK=$(cat ~/.agora-ingest-token 2>/dev/null); \
                 [ -n \"$R\" ] && [ -n \"$TOK\" ] && \
                   curl -s -m 5 -X POST -H \"x-agora-token: $TOK\" -H 'Content-Type: application/json' \
                   {hub}/stale -d \"{{\\\"room\\\":\\\"$R\\\",\\\"name\\\":\\\"{name}\\\"}}\" >/dev/null 2>&1; \
                 echo killed agora-{name}"
            );
            let (ok, out) = run(host, &script);
            print!("{out}");
            std::process::exit(if ok { 0 } else { 1 });
        }
        "restart" => {
            let script = format!(
                "test -f ~/agora-agents/{name}/.agora-spawn || {{ echo __NOSPAWN__; exit 9; }}; \
                 tmux kill-session -t agora-{name} 2>/dev/null; \
                 read -r H R < ~/agora-agents/{name}/.agora-spawn && echo \"$H $R\""
            );
            let (ok, out) = run(host, &script);
            if !ok || out.contains("__NOSPAWN__") {
                println!("cannot restart '{name}': not an agora-spawned agent (no spawn record). Only agents created with /spawn can be restarted.");
                std::process::exit(1);
            }
            let mut parts = out.split_whitespace();
            let h = parts.next().unwrap_or("claude").to_string();
            let r = parts.next().unwrap_or("dev").to_string();
            // re-exec ourselves with the recorded args
            let me = std::env::current_exe().unwrap();
            let mut c = Command::new(me);
            c.args(["spawn", &name, "--harness", &h, "--room", &r]);
            if let Some(hst) = host {
                c.args(["--on", hst]);
            }
            let status = c.status().map(|s| s.success()).unwrap_or(false);
            std::process::exit(if status { 0 } else { 1 });
        }
        "agents" => {
            let (_, out) = run(host, "tmux ls 2>/dev/null | grep '^agora-' || echo '(no agora agents)'");
            print!("{out}");
        }
        _ => {
            eprintln!("usage: orch <spawn|kill|restart|agents> [name] [--harness h] [--room r] [--on user@host]");
            std::process::exit(1);
        }
    }
}

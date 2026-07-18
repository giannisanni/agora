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
            // non-interactive launch flags so first-run trust/permission
            // prompts don't block a headless (esp. remote) spawn.
            let bin = match harness.as_str() {
                "claude" | "claude-code" =>
                    format!("claude --permission-mode acceptEdits --dangerously-skip-permissions{m}"),
                "codex" => format!("codex --dangerously-bypass-approvals-and-sandbox{m_codex}"),
                "opencode" => format!("opencode run{m}"),
                other => other.to_string(),
            };
            let prompt = join_prompt(&name, &room, &harness);
            // self-healing wrapper: if the agent process exits (turn ends /
            // crashes), restart it after a short backoff — keeps the agent
            // resident and the tmux session (and server) alive. `.agora-stop`
            // breaks the loop for a clean kill.
            // respawn loop with exponential backoff: a fast-exiting agent (crash,
            // usage limit) backs off 5→10→...→300s instead of hammering; a run
            // that lasted >60s resets the backoff.
            let run_sh = format!(
                "#!/bin/bash\ncd \"$(dirname \"$0\")\"\nb=5\nwhile [ ! -f .agora-stop ]; do\n  s=$SECONDS\n  {bin} \"$(cat prompt.txt)\"\n  [ -f .agora-stop ] && break\n  if [ $((SECONDS - s)) -ge 60 ]; then b=5; else b=$((b*2)); [ $b -gt 300 ] && b=300; fi\n  echo \"[agora] agent exited; restarting in ${{b}}s\"\n  sleep $b\ndone\n"
            );
            let script = format!(
                "mkdir -p ~/agora-agents/{name} && cd ~/agora-agents/{name} && \
                 printf '%s %s\\n' '{harness}' '{room}' > .agora-spawn && \
                 rm -f .agora-stop && \
                 printf '%s' {prompt_q} > prompt.txt && \
                 printf '%s' {run_q} > run.sh && \
                 tmux new-session -d -s agora-{name} 'bash run.sh' && \
                 echo spawned agora-{name}",
                prompt_q = sh_squote(&prompt),
                run_q = sh_squote(&run_sh),
            );
            let (ok, out) = run(host, &script);
            print!("{out}");
            std::process::exit(if ok { 0 } else { 1 });
        }
        "kill" => {
            // set the stop flag first so the respawn loop won't relaunch, then
            // kill the session; drop the id-file so a reused id can't mis-map.
            let (ok, out) = run(host, &format!(
                "touch ~/agora-agents/{name}/.agora-stop 2>/dev/null; tmux kill-session -t agora-{name}; rm -f ~/agora-agents/{name}/.agora-agent-id; echo killed agora-{name}"
            ));
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

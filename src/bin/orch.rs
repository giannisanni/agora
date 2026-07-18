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

fn run(host: Option<&str>, script: &str) -> (bool, String) {
    let out = match host {
        Some(h) => Command::new("ssh").args([h, script]).output(),
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

fn join_prompt(name: &str, room: &str) -> String {
    format!(
        "Join agora room \"{room}\" as \"{name}\" using the agora MCP tools. \
         Write your agent_id to .agora-agent-id in your cwd. Set your status \
         with set_status. Check your inbox and handle anything there. Then \
         loop forever: park in wait_for_messages (timeout_secs 240), handle \
         whatever arrives (reply, do tasks, update status), and park again. \
         You are a resident agent; do not end your turn."
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let mut name = String::new();
    let mut harness = "claude".to_string();
    let mut room = "dev".to_string();
    let mut host: Option<String> = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--harness" => harness = it.next().cloned().unwrap_or(harness),
            "--room" => room = it.next().cloned().unwrap_or(room),
            "--on" => host = it.next().cloned(),
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
            let bin = match harness.as_str() {
                "claude" | "claude-code" => "claude",
                "codex" => "codex",
                other => other, // any CLI that takes a prompt argument
            };
            let prompt = join_prompt(&name, &room).replace('"', "\\\"");
            let script = format!(
                "mkdir -p ~/agora-agents/{name} && cd ~/agora-agents/{name} && \
                 printf '%s %s\\n' '{harness}' '{room}' > .agora-spawn && \
                 tmux new-session -d -s agora-{name} \"{bin} \\\"{prompt}\\\"\" && \
                 echo spawned agora-{name}"
            );
            let (ok, out) = run(host, &script);
            print!("{out}");
            std::process::exit(if ok { 0 } else { 1 });
        }
        "kill" => {
            let (ok, out) = run(host, &format!("tmux kill-session -t agora-{name} && echo killed agora-{name}"));
            print!("{out}");
            std::process::exit(if ok { 0 } else { 1 });
        }
        "restart" => {
            let script = format!(
                "cd ~/agora-agents/{name} 2>/dev/null && read -r H R < .agora-spawn && \
                 tmux kill-session -t agora-{name} 2>/dev/null; \
                 cd ~/agora-agents/{name} && read -r H R < .agora-spawn && \
                 echo \"$H $R\""
            );
            let (ok, out) = run(host, &script);
            if !ok {
                eprintln!("restart: no spawn record for {name}: {out}");
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

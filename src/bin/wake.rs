//! agora wake shim: wakes idle local agents that have unread mail by typing a
//! nudge into their terminal. Terminal adapters: tmux, Mosaic, iTerm2.
//!
//! Mapping: agents drop `.agora-agent-id` in their cwd; the shim scans
//! AGORA_WAKE_DIRS (colon-separated, default ~/workspace) for those files,
//! intersects with the hub's /wakeable list (idle + unread), finds a local
//! terminal whose foreground process cwd matches, and types the nudge.
//! ponytail: nudges any terminal at that cwd without proving an agent runs
//! there; per-process verification if misfires ever happen.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

const POLL_SECS: u64 = 20;
const COOLDOWN_SECS: u64 = 180;

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn nudge_text(unread: i64) -> String {
    format!(
        "agora wake: you have {unread} unread message(s). Check your agora inbox, respond to what you find, then park in wait_for_messages instead of going idle."
    )
}

/// cwd -> agent_id from .agora-agent-id files under the configured dirs.
fn local_agents(dirs: &str) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for dir in dirs.split(':').filter(|d| !d.is_empty()) {
        let found = sh("find", &[dir, "-maxdepth", "3", "-name", ".agora-agent-id", "-type", "f"]);
        for path in found.lines() {
            if let Ok(content) = std::fs::read_to_string(path) {
                let id: String = content.chars().filter(|c| c.is_ascii_digit()).collect();
                if let (Ok(id), Some(cwd)) = (id.parse::<i64>(), path.strip_suffix("/.agora-agent-id")) {
                    map.insert(cwd.to_string(), id);
                }
            }
        }
    }
    map
}

/// tty -> cwd of the most interesting process on it (agent binaries first).
fn tty_cwd(tty: &str) -> Option<String> {
    let ps = sh("ps", &["-t", tty, "-o", "pid=,comm="]);
    let mut pids: Vec<(i32, String)> = ps
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.to_string()))
        })
        .collect();
    // prefer agent-looking processes over shells
    pids.sort_by_key(|(_, comm)| {
        let c = comm.to_lowercase();
        if c.contains("claude") || c.contains("codex") || c.contains("node") { 0 } else { 1 }
    });
    for (pid, _) in pids {
        let out = sh("lsof", &["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]);
        if let Some(cwd) = out.lines().find_map(|l| l.strip_prefix('n')) {
            return Some(cwd.to_string());
        }
    }
    None
}

enum Target {
    Tmux(String),
    Mosaic(String),
    Iterm(String),
}

/// All local terminals as (target, cwd).
fn terminals() -> Vec<(Target, String)> {
    let mut out = Vec::new();
    // tmux panes expose cwd directly
    for line in sh("tmux", &["list-panes", "-a", "-F", "#{pane_id}|#{pane_current_path}"]).lines() {
        if let Some((id, cwd)) = line.split_once('|') {
            out.push((Target::Tmux(id.to_string()), cwd.to_string()));
        }
    }
    // mosaic surfaces carry tty in `tree`
    for line in sh("mosaic", &["tree", "--all"]).lines() {
        if let (Some(surf), Some(tty)) = (
            line.split_whitespace().find(|w| w.starts_with("surface:")),
            line.split_whitespace().find_map(|w| w.strip_prefix("tty=")),
        ) {
            if let Some(cwd) = tty_cwd(tty) {
                out.push((Target::Mosaic(surf.to_string()), cwd));
            }
        }
    }
    // iTerm sessions: tty|session-id via AppleScript
    let script = r#"tell application "iTerm2"
set out to ""
repeat with w in windows
repeat with t in tabs of w
repeat with s in sessions of t
set out to out & (tty of s) & "|" & (id of s) & linefeed
end repeat
end repeat
end repeat
return out
end tell"#;
    for line in sh("osascript", &["-e", script]).lines() {
        if let Some((tty, sid)) = line.split_once('|') {
            let tty = tty.trim().strip_prefix("/dev/").unwrap_or(tty.trim());
            if let Some(cwd) = tty_cwd(tty) {
                out.push((Target::Iterm(sid.trim().to_string()), cwd));
            }
        }
    }
    out
}

fn wake(target: &Target, text: &str) -> bool {
    match target {
        Target::Tmux(id) => {
            Command::new("tmux").args(["send-keys", "-t", id, "-l", text]).status().is_ok()
                && Command::new("tmux").args(["send-keys", "-t", id, "Enter"]).status().is_ok()
        }
        Target::Mosaic(surf) => {
            Command::new("mosaic").args(["send", "--surface", surf, text]).status().is_ok()
                && Command::new("mosaic").args(["send-key", "--surface", surf, "Enter"]).status().is_ok()
        }
        Target::Iterm(sid) => {
            let script = format!(
                r#"tell application "iTerm2"
repeat with w in windows
repeat with t in tabs of w
repeat with s in sessions of t
if (id of s as text) is "{sid}" then
tell s to write text "{text}"
end if
end repeat
end repeat
end repeat
end tell"#,
                sid = sid,
                text = text.replace('"', "\\\""),
            );
            Command::new("osascript").args(["-e", &script]).status().is_ok()
        }
    }
}

fn main() {
    let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
    let home = std::env::var("HOME").expect("HOME not set");
    let dirs = std::env::var("AGORA_WAKE_DIRS").unwrap_or_else(|_| format!("{home}/workspace"));
    let token = std::env::var("AGORA_INGEST_TOKEN")
        .ok()
        .or_else(|| std::fs::read_to_string(format!("{home}/.agora-ingest-token")).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            eprintln!("no token: set AGORA_INGEST_TOKEN or ~/.agora-ingest-token");
            std::process::exit(1);
        });
    let once = std::env::args().any(|a| a == "--once");
    println!("agora wake: hub={hub} dirs={dirs}");

    let mut last_wake: HashMap<i64, Instant> = HashMap::new();
    loop {
        let wakeable: Vec<serde_json::Value> = ureq::get(&format!("{hub}/wakeable"))
            .header("x-agora-token", &token)
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_json::<serde_json::Value>().ok())
            .and_then(|v| v["agents"].as_array().cloned())
            .unwrap_or_default();

        if !wakeable.is_empty() {
            let locals = local_agents(&dirs);
            let terms = terminals();
            for agent in &wakeable {
                let id = agent["agent_id"].as_i64().unwrap_or(0);
                let unread = agent["unread"].as_i64().unwrap_or(0);
                let Some(cwd) = locals.iter().find(|(_, aid)| **aid == id).map(|(c, _)| c.clone()) else {
                    continue; // not an agent on this machine
                };
                if last_wake.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(COOLDOWN_SECS)) {
                    continue;
                }
                if let Some((target, _)) = terms.iter().find(|(_, tcwd)| *tcwd == cwd) {
                    if wake(target, &nudge_text(unread)) {
                        println!("woke agent {id} at {cwd} ({unread} unread)");
                        last_wake.insert(id, Instant::now());
                    }
                }
            }
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(POLL_SECS));
    }
}

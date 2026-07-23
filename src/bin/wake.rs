//! agora wake shim: wakes idle local agents that have unread mail by typing a
//! nudge into their terminal. Built-in adapters: tmux, Mosaic, iTerm2, Apple
//! Terminal. For anything else (WezTerm, Kitty, Alacritty, Cline, ssh-tmux …)
//! drop an executable in ~/.config/agora/injectors/ — see run_injectors().
//!
//! Mapping: agents drop `.agora-agent-id` in their cwd; the shim scans
//! AGORA_WAKE_DIRS (colon-separated, default $HOME), intersects with the hub's
//! /wakeable list (idle + unread), finds a local terminal whose foreground
//! process cwd matches, and types the nudge.
//! ponytail: nudges any terminal at that cwd without proving an agent runs
//! there; per-process verification if misfires ever happen.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

const POLL_SECS: u64 = 5;    // fast enough that a DM feels like a push
const COOLDOWN_SECS: u64 = 45;

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Most recent inbox-class message addressed to `name` (or broadcast), from
/// the non-consuming /messages view — used to preview content in the nudge.
fn latest_for(hub: &str, token: &str, room: &str, name: &str) -> Option<(String, String)> {
    let v: serde_json::Value = ureq::get(&format!("{hub}/messages?room={room}&limit=15"))
        .header("x-agora-token", token)
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    v["messages"].as_array()?.iter().rev().find_map(|m| {
        let to = m["to"].as_str();
        let from = m["from"].as_str()?;
        if from != name && (to.is_none() || to == Some(name)) {
            Some((from.to_string(), m["body"].as_str().unwrap_or("").to_string()))
        } else {
            None
        }
    })
}

fn nudge_text(unread: i64, preview: Option<(String, String)>) -> String {
    match preview {
        Some((from, body)) => {
            let short: String = body.chars().take(160).collect();
            format!(
                "agora: new message from {from}: \"{short}\" ({unread} unread total). \
                 Check your agora inbox and respond, then park in wait_for_messages."
            )
        }
        None => format!(
            "agora: you have {unread} unread message(s). Check your agora inbox, respond, then park in wait_for_messages."
        ),
    }
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
    Terminal(String), // Apple Terminal.app tty
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
    // Apple Terminal.app: keyed by tty, which we match to a cwd
    let term_script = r#"tell application "Terminal"
set out to ""
repeat with w in windows
repeat with t in tabs of w
set out to out & (tty of t) & linefeed
end repeat
end repeat
return out
end tell"#;
    for line in sh("osascript", &["-e", term_script]).lines() {
        let tty_full = line.trim();
        let tty = tty_full.strip_prefix("/dev/").unwrap_or(tty_full);
        if !tty.is_empty() {
            if let Some(cwd) = tty_cwd(tty) {
                out.push((Target::Terminal(tty_full.to_string()), cwd));
            }
        }
    }
    out
}

/// Bring a terminal to the foreground (best-effort per app).
fn bring_to_front(target: &Target) -> bool {
    match target {
        Target::Terminal(tty) => {
            let script = format!(
                r#"tell application "Terminal"
activate
repeat with w in windows
repeat with t in tabs of w
if (tty of t) is "{tty}" then
set selected tab of w to t
set index of w to 1
end if
end repeat
end repeat
end tell"#
            );
            Command::new("osascript").args(["-e", &script]).status().is_ok()
        }
        Target::Iterm(sid) => {
            let script = format!(
                r#"tell application "iTerm2"
activate
repeat with w in windows
repeat with t in tabs of w
repeat with s in sessions of t
if (id of s as text) is "{sid}" then
select t
select w
end if
end repeat
end repeat
end repeat
end tell"#
            );
            Command::new("osascript").args(["-e", &script]).status().is_ok()
        }
        Target::Mosaic(surf) => {
            // focus the surface's pane, flash it, and raise the app
            let _ = Command::new("mosaic").args(["focus-pane", "--pane", surf]).status();
            let _ = Command::new("mosaic").args(["trigger-flash", "--surface", surf]).status();
            Command::new("open").args(["-a", "Mosaic"]).status().is_ok()
        }
        Target::Tmux(pane) => {
            let _ = Command::new("tmux").args(["select-window", "-t", pane]).status();
            let session = sh("tmux", &["display-message", "-p", "-t", pane, "#{session_name}"]);
            let session = session.trim();
            if session.is_empty() {
                return false;
            }
            let attached = !sh("tmux", &["list-clients", "-t", session]).trim().is_empty();
            if attached {
                // already showing somewhere: raise Terminal (best-effort host)
                return Command::new("open").args(["-a", "Terminal"]).status().is_ok();
            }
            // detached: open a real Terminal window attached to the session so
            // the user actually SEES the agent
            let script = format!(
                r#"tell application "Terminal"
activate
do script "tmux attach -t {session}"
end tell"#
            );
            Command::new("osascript").args(["-e", &script]).status().is_ok()
        }
    }
}

/// Pluggable injectors (AMQ-style): any executable in the injectors dir
/// (AGORA_INJECTORS_DIR, default ~/.config/agora/injectors) is a fallback for
/// terminals the built-in adapters don't cover. Contract: called as
/// `injector <cwd> <agent_name>` with the nudge text on stdin; exit 0 means
/// "I found a terminal at that cwd and delivered the message". First success
/// wins. This lets people add WezTerm/Kitty/Cline/ssh-tmux/etc. without
/// recompiling the shim.
fn injectors_dir() -> std::path::PathBuf {
    std::env::var("AGORA_INJECTORS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".config/agora/injectors")
        })
}

fn run_injectors(cwd: &str, name: &str, text: &str) -> bool {
    let dir = injectors_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else { return false };
    let mut scripts: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && std::fs::metadata(p).map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o111 != 0
        }).unwrap_or(false))
        .collect();
    scripts.sort(); // deterministic order
    for script in scripts {
        use std::io::Write;
        let child = Command::new(&script)
            .arg(cwd)
            .arg(name)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(si) = c.stdin.as_mut() { let _ = si.write_all(text.as_bytes()); }
            if c.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
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
        Target::Terminal(tty) => {
            // `do script ... in tab whose tty` types + runs in that Terminal tab
            let script = format!(
                r#"tell application "Terminal"
repeat with w in windows
repeat with t in tabs of w
if (tty of t) is "{tty}" then
do script "{text}" in t
end if
end repeat
end repeat
end tell"#,
                tty = tty,
                text = text.replace('\\', "\\\\").replace('"', "\\\""),
            );
            Command::new("osascript").args(["-e", &script]).status().is_ok()
        }
    }
}

/// `wake reveal <agent_id>`: find that agent's local terminal and raise it.
fn reveal(agent_id: i64, dirs: &str) {
    let cwd = match local_agents(dirs).into_iter().find(|(_, id)| *id == agent_id) {
        Some((cwd, _)) => cwd,
        None => {
            println!("not local (no .agora-agent-id for agent {agent_id} under {dirs})");
            return;
        }
    };
    match terminals().into_iter().find(|(_, tcwd)| *tcwd == cwd) {
        Some((target, _)) if bring_to_front(&target) => println!("revealed agent {agent_id} at {cwd}"),
        Some(_) => println!("found terminal but could not raise it"),
        None => println!("no open terminal at {cwd}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
    let home = std::env::var("HOME").expect("HOME not set");
    // wake only reads id-files (no transcript content), so it can safely scan
    // all of $HOME — agents live in ~/workspace, ~/agora-agents, anywhere.
    let dirs = std::env::var("AGORA_WAKE_DIRS").unwrap_or_else(|_| home.clone());

    if args.first().map(String::as_str) == Some("reveal") {
        let id: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(-1);
        reveal(id, &dirs);
        return;
    }
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
                let name = agent["name"].as_str().unwrap_or("");
                let room = agent["room"].as_str().unwrap_or("dev");
                let Some(cwd) = locals.iter().find(|(_, aid)| **aid == id).map(|(c, _)| c.clone()) else {
                    continue; // not an agent on this machine
                };
                if last_wake.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(COOLDOWN_SECS)) {
                    continue;
                }
                let preview = latest_for(&hub, &token, room, name);
                let nudge = nudge_text(unread, preview);
                // built-in adapters first; then pluggable injector scripts.
                let woke = match terms.iter().find(|(_, tcwd)| *tcwd == cwd) {
                    Some((target, _)) => wake(target, &nudge),
                    None => false,
                } || run_injectors(&cwd, name, &nudge);
                if woke {
                    println!("woke agent {id} ({name}) at {cwd} ({unread} unread)");
                    last_wake.insert(id, Instant::now());
                }
            }
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(POLL_SECS));
    }
}

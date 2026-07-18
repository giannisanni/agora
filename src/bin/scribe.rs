//! agora scribe: tails local agent transcripts and mirrors recent turns into a
//! room as ambient feed entries (kind=summary). Stateless by design — the
//! hub's source_id dedup makes every re-scan idempotent.
//!
//! Parsing rules for Claude Code follow Mosaic's ClaudeTranscriptFileParser:
//! read only the file tail, drop the first (possibly truncated) line, keep only
//! real user/assistant text, skip meta lines, subagent sidechains, tool-only
//! content, and slash-command echoes.
//!
//! Loop safety: agora delivers messages through MCP tool results, which appear
//! in transcripts as tool content — filtered out below. Mirrored turns are
//! feed-class, which never enters any inbox. So the scribe cannot echo agora
//! traffic back into agora.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TAIL_BYTES: u64 = 512 * 1024;
const MAX_TURNS_PER_FILE: usize = 20;
const MAX_BODY_CHARS: usize = 1200;
const ACTIVE_WITHIN_SECS: u64 = 900; // only mirror files modified recently
const POLL_SECS: u64 = 15;

struct Turn {
    source_id: String,
    role: &'static str,
    text: String,
}

fn main() {
    let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
    let room = std::env::var("AGORA_ROOM").unwrap_or_else(|_| {
        eprintln!("AGORA_ROOM not set");
        std::process::exit(1);
    });
    let machine = std::env::var("AGORA_MACHINE").unwrap_or_else(|_| hostname());
    let home = std::env::var("HOME").expect("HOME not set");
    let once = std::env::args().any(|a| a == "--once");
    // privacy boundary: only sessions whose cwd is under one of these path
    // prefixes are mirrored. Unset AGORA_DIRS = mirror nothing (opt-in, not
    // opt-out: an always-on daemon must never leak personal sessions).
    let dirs: Vec<String> = std::env::var("AGORA_DIRS")
        .map(|v| v.split(',').map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        eprintln!("AGORA_DIRS not set (comma-separated cwd prefixes to mirror); refusing to mirror everything");
        std::process::exit(1);
    }

    println!("scribe: room={room} hub={hub} machine={machine} dirs={dirs:?}");
    let mut cycle: u32 = 0;
    loop {
        // usage report every ~60s: rolling-5h token totals per harness
        if cycle % 4 == 0 {
            report_usage(&hub, &machine, &home);
        }
        cycle = cycle.wrapping_add(1);
        let mut posted = 0;
        for file in active_files(&format!("{home}/.claude/projects"), "jsonl") {
            posted += mirror(&hub, &room, &machine, "claude-code", &file, parse_claude, &dirs);
        }
        for file in active_files(&format!("{home}/.codex/sessions"), "jsonl") {
            posted += mirror(&hub, &room, &machine, "codex", &file, parse_codex, &dirs);
        }
        if posted > 0 {
            println!("scribe: mirrored {posted} turns");
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(POLL_SECS));
    }
}

/// Sum of input+output tokens in transcripts touched within the last 5h.
/// ponytail: file-mtime gate, whole-tail sums — an approximation (old lines in
/// a live file count); good enough as a routing signal, not billing.
fn cc_tokens_5h(home: &str) -> i64 {
    let mut total = 0i64;
    let found = sh("find", &[&format!("{home}/.claude/projects"), "-name", "*.jsonl", "-mmin", "-300", "-type", "f"]);
    for path in found.lines() {
        for line in read_tail(std::path::Path::new(path)).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let u = &v["message"]["usage"];
                total += u["input_tokens"].as_i64().unwrap_or(0) + u["output_tokens"].as_i64().unwrap_or(0);
            }
        }
    }
    total
}

fn codex_tokens_5h(home: &str) -> i64 {
    let mut total = 0i64;
    let found = sh("find", &[&format!("{home}/.codex/sessions"), "-name", "*.jsonl", "-mmin", "-300", "-type", "f"]);
    for path in found.lines() {
        // last token_count event carries the session-cumulative total
        let mut last = 0i64;
        for line in read_tail(std::path::Path::new(path)).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v["payload"]["type"].as_str() == Some("token_count") {
                    if let Some(t) = v["payload"]["info"]["total_token_usage"]["total_tokens"].as_i64() {
                        last = t;
                    }
                }
            }
        }
        total += last;
    }
    total
}

fn report_usage(hub: &str, machine: &str, home: &str) {
    let token = std::env::var("AGORA_INGEST_TOKEN").unwrap_or_default();
    for (harness, tokens) in [("claude-code", cc_tokens_5h(home)), ("codex", codex_tokens_5h(home))] {
        let payload = serde_json::json!({ "machine": machine, "harness": harness, "tokens_5h": tokens });
        let _ = ureq::post(&format!("{hub}/usage"))
            .header("x-agora-token", &token)
            .send_json(&payload);
    }
}

fn sh(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Recursively find files with `ext` modified within ACTIVE_WITHIN_SECS.
fn active_files(root: &str, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .and_then(|t| t.elapsed().map_err(|_| std::io::Error::other("clock")))
                    .is_ok_and(|age| age.as_secs() < ACTIVE_WITHIN_SECS)
            {
                out.push(path);
            }
        }
    }
    out
}

fn mirror(
    hub: &str,
    room: &str,
    machine: &str,
    harness: &str,
    file: &Path,
    parse: fn(&str) -> Vec<Turn>,
    dirs: &[String],
) -> usize {
    let session = file.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    let short = &session[..session.len().min(8)];
    // one mirror identity per machine (not per harness) — the scribe is a plain
    // daemon, not an agent. Harness/session stays visible in each turn's prefix.
    let name = format!("{machine}-scribe");
    let tail = read_tail(file);
    let cwd = match harness {
        "claude-code" => cwd_from_lines(&tail),
        _ => cwd_from_head(file),
    };
    // unknown cwd -> skip: never mirror what we can't attribute to a project
    let Some(cwd) = cwd else { return 0 };
    if !dirs.iter().any(|d| cwd == *d || cwd.starts_with(&format!("{d}/"))) {
        return 0;
    }
    let turns = parse(&tail);
    let mut posted = 0;
    for turn in turns.iter().rev().take(MAX_TURNS_PER_FILE).rev() {
        let body: String = format!("[{harness} {short}] {}: {}", turn.role, turn.text)
            .chars()
            .take(MAX_BODY_CHARS)
            .collect();
        let payload = serde_json::json!({
            "room": room,
            "name": name,
            "harness": "scribe",
            "mirror": true,
            "machine": machine,
            "body": body,
            "kind": "summary",
            "source_id": turn.source_id,
        });
        let token = std::env::var("AGORA_INGEST_TOKEN").unwrap_or_default();
        match ureq::post(&format!("{hub}/ingest"))
            .header("x-agora-token", &token)
            .send_json(&payload)
        {
            Ok(mut resp) => {
                let new = resp
                    .body_mut()
                    .read_json::<serde_json::Value>()
                    .is_ok_and(|v| v["new"].as_bool() == Some(true));
                if new {
                    posted += 1;
                }
            }
            Err(e) => {
                eprintln!("scribe: ingest failed: {e}");
                return posted;
            }
        }
    }
    posted
}

/// First `cwd` field found in jsonl lines (Claude Code puts it on many line types).
fn cwd_from_lines(jsonl: &str) -> Option<String> {
    jsonl.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("cwd")?
            .as_str()
            .map(String::from)
    })
}

/// Codex: `session_meta` head line carries payload.cwd.
fn cwd_from_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let first = head.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    v["payload"]["cwd"].as_str().map(String::from)
}

fn read_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    if start > 0 {
        // tail window almost certainly starts mid-line; drop the fragment
        if let Some(nl) = buf.find('\n') {
            buf = buf[nl + 1..].to_string();
        }
    }
    buf
}

/// Claude Code: `~/.claude/projects/<slug>/<session>.jsonl`
fn parse_claude(jsonl: &str) -> Vec<Turn> {
    let mut turns = Vec::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let role = match v["type"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if v["isMeta"].as_bool() == Some(true) || v["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        let Some(text) = conversational_text(&v["message"]["content"]) else { continue };
        if text.starts_with("<command-") || text.starts_with("<local-command-") {
            continue;
        }
        let source_id = v["uuid"].as_str().map(String::from).unwrap_or_else(|| hash_id(&text));
        turns.push(Turn { source_id, role, text });
    }
    turns
}

fn conversational_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    let blocks = content.as_array()?;
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|b| b["type"].as_str() == Some("text"))
        .filter_map(|b| b["text"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, event_msg lines with
/// payload.type user_message / agent_message.
fn parse_codex(jsonl: &str) -> Vec<Turn> {
    let mut turns = Vec::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"].as_str() != Some("event_msg") {
            continue;
        }
        let role = match v["payload"]["type"].as_str() {
            Some("user_message") => "user",
            Some("agent_message") => "assistant",
            _ => continue,
        };
        let Some(text) = v["payload"]["message"].as_str().map(str::trim).filter(|t| !t.is_empty())
        else {
            continue;
        };
        // codex lines carry no uuid; timestamp + text hash is stable across re-scans
        let ts = v["timestamp"].as_str().unwrap_or("");
        let source_id = format!("cx-{ts}-{}", hash_id(text));
        turns.push(Turn { source_id, role, text: text.to_string() });
    }
    turns
}

fn hash_id(text: &str) -> String {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_parser_filters_noise() {
        let jsonl = r#"{"type":"user","uuid":"u1","message":{"content":"real question"}}
{"type":"user","uuid":"u2","isMeta":true,"message":{"content":"meta noise"}}
{"type":"assistant","uuid":"u3","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent"}]}}
{"type":"assistant","uuid":"u4","message":{"content":[{"type":"tool_use","name":"Bash"}]}}
{"type":"assistant","uuid":"u5","message":{"content":[{"type":"text","text":"real answer"}]}}
{"type":"user","uuid":"u6","message":{"content":"<command-name>/model</command-name>"}}"#;
        let turns = parse_claude(jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "real question");
        assert_eq!(turns[1].text, "real answer");
        assert_eq!(turns[1].source_id, "u5");
    }

    #[test]
    fn cwd_extraction_and_prefix_semantics() {
        let jsonl = r#"{"type":"attachment","cwd":"/Users/g/workspace/agora"}
{"type":"user","uuid":"u1","message":{"content":"hi"}}"#;
        assert_eq!(cwd_from_lines(jsonl).as_deref(), Some("/Users/g/workspace/agora"));
        assert_eq!(cwd_from_lines(r#"{"type":"user"}"#), None);

        // prefix must match on path boundaries: /Users/g/work must NOT match /Users/g/workspace
        let dirs = vec!["/Users/g/work".to_string()];
        let matches = |cwd: &str| dirs.iter().any(|d| cwd == *d || cwd.starts_with(&format!("{d}/")));
        assert!(matches("/Users/g/work"));
        assert!(matches("/Users/g/work/x"));
        assert!(!matches("/Users/g/workspace/agora"));
    }

    #[test]
    fn codex_parser_extracts_messages() {
        let jsonl = r#"{"timestamp":"t1","type":"event_msg","payload":{"type":"task_started"}}
{"timestamp":"t2","type":"event_msg","payload":{"type":"user_message","message":"the secret word is lol"}}
{"timestamp":"t3","type":"event_msg","payload":{"type":"agent_message","message":"Got it."}}
{"timestamp":"t4","type":"response_item","payload":{"type":"message","role":"developer","content":[]}}"#;
        let turns = parse_codex(jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");
        // same input -> same source_id (idempotent re-scan)
        assert_eq!(parse_codex(jsonl)[0].source_id, turns[0].source_id);
    }
}

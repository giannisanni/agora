//! agora TUI: live room command center.
//! Left: message timeline (collapsed to 5 lines; click or ↑/↓ + Enter to
//! expand). Right: peers. Bottom: input.
//! Keys: Tab feed/messages, Enter send (or toggle expand when input empty and
//! a message is selected), ↑/↓ select, Esc clear, q / Ctrl-C quit.
//! Slash commands: /rooms /room /move /kick /spawn /usage /name <me>
//! Env: AGORA_HUB, AGORA_ROOM, AGORA_NAME; token from ~/.agora-ingest-token.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

const COLLAPSE_AT: usize = 5; // bodies longer than this collapse to 4 + hint

// (name, args hint, description) — powers the / autocomplete popup
const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "Show available commands"),
    ("rooms", "", "List all rooms with agent/message counts"),
    ("room", "<name>", "Switch to a room (creates it on first post)"),
    ("move", "<agent> <room>", "Move an agent from this room to another"),
    ("name", "<me>", "Change the name you post as"),
    ("kick", "<agent> [agent...]", "Remove agent(s) from this room"),
    ("spawn", "<name> [harness] [host]", "Spawn a resident agent in tmux (here or user@host)"),
    ("agents", "", "List spawned tmux agents"),
    ("killagent", "<name> [host]", "Kill a spawned tmux agent"),
    ("restart", "<name> [host]", "Restart a spawned tmux agent"),
    ("usage", "", "Show 5h token usage per machine/harness"),
    ("park", "<agent> <secs>", "Set an agent's idle park timeout (1-3600s)"),
    ("resident", "<agent>", "Tell an agent to stay resident (loop wait_for_messages)"),
    ("idle", "<agent>", "Tell an agent to go idle (stop looping; wake shim revives it)"),
    ("delroom", "<room>", "Delete a room (all its agents + messages)"),
    ("quit", "", "Exit the TUI"),
];

struct App {
    hub: String,
    room: String,
    name: String,
    token: String,
    messages: Vec<serde_json::Value>,
    feed: Vec<serde_json::Value>,
    peers: Vec<serde_json::Value>,
    input: String,
    status: String,
    show_feed: bool,
    expanded: HashSet<String>,   // keys of expanded messages ("m:<id>" / "f:<id>")
    selected: Option<usize>,     // index into current list (messages or feed)
    vis: Vec<(usize, u16, u16)>, // (msg index, y, height) of visible msgs, for click hit-testing
    msg_area: Rect,
    suggest_idx: usize,
    quit: bool,
    rooms: Vec<String>,
    peer_vis: Vec<(usize, u16, u16)>, // (peer idx, y, h) for clicks
    peers_area: Rect,
    peer_menu: Option<usize>,         // open menu for peer idx
    menu_items: Vec<(Rect, MenuAction)>,
    peers_focused: bool,              // ←/→ switches focus between panes
    peer_sel: usize,                  // keyboard-selected peer
    menu_sel: usize,                  // keyboard-selected menu item
}

#[derive(Clone, Copy)]
enum MenuAction {
    Message,
    Reveal,
    Kick,
    Move,
    Close,
}

struct Suggestion {
    completed: String, // input becomes this on completion
    label: String,
    desc: String,
    run: bool,         // execute immediately on Enter
}

fn get(url: &str, token: &str) -> Option<serde_json::Value> {
    ureq::get(url)
        .header("x-agora-token", token)
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()
}

fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in s.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if line.is_empty() {
                line = word.to_string();
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
            while line.chars().count() > width {
                let head: String = line.chars().take(width).collect();
                line = line.chars().skip(width).collect();
                out.push(head);
            }
        }
        out.push(line);
    }
    out
}

impl App {
    fn current_list(&self) -> &Vec<serde_json::Value> {
        if self.show_feed { &self.feed } else { &self.messages }
    }

    fn msg_key(&self, m: &serde_json::Value) -> String {
        let prefix = if self.show_feed { "f" } else { "m" };
        format!("{prefix}:{}", m["id"].as_i64().unwrap_or(0))
    }

    fn heartbeat(&self) {
        let payload = serde_json::json!({
            "room": self.room, "name": self.name, "harness": "human-tui", "machine": "tui",
        });
        let _ = ureq::post(&format!("{}/heartbeat", self.hub))
            .header("x-agora-token", &self.token)
            .send_json(&payload);
    }

    fn refresh(&mut self) {
        self.heartbeat();
        if let Some(v) = get(&format!("{}/messages?room={}&limit=100", self.hub, self.room), &self.token) {
            self.messages = v["messages"].as_array().cloned().unwrap_or_default();
        }
        if let Some(v) = get(&format!("{}/feed?room={}&limit=100", self.hub, self.room), &self.token) {
            self.feed = v["feed"].as_array().cloned().unwrap_or_default();
        }
        if let Some(v) = get(&format!("{}/peers?room={}", self.hub, self.room), &self.token) {
            self.peers = v["peers"].as_array().cloned().unwrap_or_default();
        }
    }

    fn toggle(&mut self, idx: usize) {
        let Some(m) = self.current_list().get(idx) else { return };
        let key = self.msg_key(m);
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            // empty input + selection = toggle expand
            if let Some(i) = self.selected {
                self.toggle(i);
            }
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd.to_string());
            self.input.clear();
            return;
        }
        // leading @names (any number) = recipients; rest = body
        let mut recipients: Vec<String> = Vec::new();
        let mut body_words: Vec<&str> = Vec::new();
        for word in text.split_whitespace() {
            if body_words.is_empty() {
                if let Some(n) = word.strip_prefix('@') {
                    recipients.push(n.to_string());
                    continue;
                }
            }
            body_words.push(word);
        }
        let body = body_words.join(" ");
        if body.is_empty() {
            self.status = "empty message".into();
            return;
        }
        let targets: Vec<Option<String>> = if recipients.is_empty() {
            vec![None]
        } else {
            recipients.iter().cloned().map(Some).collect()
        };
        let mut sent = 0;
        for to in &targets {
            let payload = serde_json::json!({
                "room": self.room, "name": self.name, "harness": "human-tui",
                "machine": "tui", "body": body, "kind": "msg", "to": to,
            });
            match ureq::post(&format!("{}/ingest", self.hub))
                .header("x-agora-token", &self.token)
                .send_json(&payload)
            {
                Ok(_) => sent += 1,
                Err(e) => { self.status = format!("send failed: {e}"); return; }
            }
        }
        self.status = if recipients.is_empty() {
            "sent".into()
        } else {
            format!("sent → {} ({sent} DMs)", recipients.join(", "))
        };
        self.input.clear();
        self.refresh();
    }

    fn dm_control(&mut self, agent: &str, text: &str) {
        let payload = serde_json::json!({
            "room": self.room, "name": self.name, "harness": "human-tui",
            "machine": "tui", "body": text, "kind": "msg", "to": agent,
        });
        match ureq::post(&format!("{}/ingest", self.hub))
            .header("x-agora-token", &self.token)
            .send_json(&payload)
        {
            Ok(_) => { self.status = format!("sent control -> {agent}"); self.refresh(); }
            Err(e) => self.status = format!("control failed: {e}"),
        }
    }

    fn orch(&mut self, args: &[String]) {
        let bin = format!("{}/workspace/agora/target/release/orch", std::env::var("HOME").unwrap_or_default());
        match std::process::Command::new(&bin).args(args).output() {
            Ok(o) => {
                let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                self.status = text.split_whitespace().collect::<Vec<_>>().join(" ");
                self.refresh();
            }
            Err(e) => self.status = format!("orch failed: {e}"),
        }
    }

    fn fetch_rooms(&mut self) {
        if let Some(v) = get(&format!("{}/rooms", self.hub), &self.token) {
            self.rooms = v["rooms"].as_array().cloned().unwrap_or_default()
                .iter()
                .filter_map(|r| r["room"].as_str().map(String::from))
                .collect();
        }
    }

    fn peer_names(&self) -> Vec<String> {
        self.peers.iter().filter_map(|p| p["name"].as_str().map(String::from)).collect()
    }

    /// Context-aware autocomplete: /commands, @peers, and argument completion
    /// for /kick /move /room /delroom.
    fn suggestions(&self) -> Vec<Suggestion> {
        let input = &self.input;
        // "/cmd" being typed
        if let Some(rest) = input.strip_prefix('/') {
            if !rest.contains(' ') {
                return COMMANDS.iter().filter(|(n, _, _)| n.starts_with(rest)).map(|(n, args, d)| Suggestion {
                    completed: if args.is_empty() { format!("/{n}") } else { format!("/{n} ") },
                    label: format!("/{n} {args}"),
                    desc: d.to_string(),
                    run: args.is_empty(),
                }).collect();
            }
            // argument completion: last token against peers or rooms
            let (cmd, tail) = rest.split_once(' ').unwrap();
            let last = tail.rsplit(' ').next().unwrap_or("");
            let prefix_done = &input[..input.len() - last.len()];
            let candidates: Vec<String> = match cmd {
                "kick" | "move" if tail.matches(' ').count() == 0 || cmd == "kick" => self.peer_names(),
                "move" => self.rooms.clone(), // second arg of /move
                "room" | "delroom" => self.rooms.clone(),
                _ => vec![],
            };
            return candidates.iter()
                .filter(|c| c.starts_with(last) && !last.is_empty() || last.is_empty())
                .filter(|c| c.starts_with(last))
                .map(|c| Suggestion {
                    completed: format!("{prefix_done}{c} "),
                    label: c.clone(),
                    desc: String::new(),
                    run: false,
                })
                .collect();
        }
        // "@name" DM completion on the last token (chains: "@a @b" both complete)
        if input.starts_with('@') {
            let last = input.rsplit(' ').next().unwrap_or("");
            if let Some(rest) = last.strip_prefix('@') {
                let prefix_done = &input[..input.len() - last.len()];
                return self.peer_names().iter()
                    .filter(|n| n.starts_with(rest))
                    .map(|n| Suggestion {
                        completed: format!("{prefix_done}@{n} "),
                        label: format!("@{n}"),
                        desc: "add DM recipient".into(),
                        run: false,
                    })
                    .collect();
            }
        }
        vec![]
    }

    fn complete_suggestion(&mut self) -> bool {
        let sugg = self.suggestions();
        let Some(s) = sugg.get(self.suggest_idx.min(sugg.len().saturating_sub(1))) else {
            return false;
        };
        if s.run {
            let cmd = s.completed.trim_start_matches('/').to_string();
            self.input.clear();
            self.command(cmd);
        } else {
            self.input = s.completed.clone();
        }
        true
    }

    fn command(&mut self, cmd: String) {
        let mut parts = cmd.split_whitespace();
        match (parts.next().unwrap_or(""), parts.next(), parts.next()) {
            ("rooms", _, _) => match get(&format!("{}/rooms", self.hub), &self.token) {
                Some(v) => {
                    let list: Vec<String> = v["rooms"].as_array().cloned().unwrap_or_default()
                        .iter()
                        .map(|r| format!("{}({}a/{}m)",
                            r["room"].as_str().unwrap_or("?"),
                            r["agents"].as_i64().unwrap_or(0),
                            r["messages"].as_i64().unwrap_or(0)))
                        .collect();
                    self.status = format!("rooms: {}", list.join("  "));
                }
                None => self.status = "rooms: fetch failed".into(),
            },
            ("room", Some(r), _) => {
                self.room = r.to_string();
                self.selected = None;
                self.fetch_rooms();
                self.refresh();
                self.status = format!("switched to room {} ({} msgs)", self.room, self.messages.len());
            }
            ("move", Some(agent), Some(to)) => {
                let payload = serde_json::json!({ "name": agent, "from": self.room, "to": to });
                match ureq::post(&format!("{}/move", self.hub))
                    .header("x-agora-token", &self.token)
                    .send_json(&payload)
                {
                    Ok(_) => { self.status = format!("moved {agent} → {to}"); self.refresh(); }
                    Err(e) => self.status = format!("move failed: {e}"),
                }
            }
            ("name", Some(n), _) => {
                let old = self.name.clone();
                let payload = serde_json::json!({ "room": self.room, "old": old, "new": n });
                let _ = ureq::post(&format!("{}/rename", self.hub))
                    .header("x-agora-token", &self.token)
                    .send_json(&payload);
                self.name = n.to_string();
                if let Ok(home) = std::env::var("HOME") {
                    let _ = std::fs::write(format!("{home}/.agora-name"), &self.name);
                }
                self.heartbeat();
                self.refresh();
                self.status = format!("renamed to {} (saved)", self.name);
            }
            ("quit", _, _) => self.quit = true,
            ("park", Some(agent), Some(secs)) => {
                let n: i64 = secs.parse().unwrap_or(600).clamp(1, 3600);
                let payload = serde_json::json!({ "room": self.room, "name": agent, "secs": n });
                match ureq::post(&format!("{}/park", self.hub))
                    .header("x-agora-token", &self.token)
                    .send_json(&payload)
                {
                    Ok(_) => self.status = format!("{agent} park timeout -> {n}s (takes effect next cycle)"),
                    Err(e) => self.status = format!("park failed: {e}"),
                }
            }
            ("resident", Some(agent), _) => {
                self.dm_control(agent, "[control] Go resident: from now on loop on wait_for_messages (omit timeout_secs to use your configured park timeout), handling messages and re-parking. Do not end your turn.");
            }
            ("idle", Some(agent), _) => {
                self.dm_control(agent, "[control] Stand down: finish any pending messages, then STOP looping and end your turn (go idle). The operator's wake shim will bring you back when new mail arrives.");
            }
            ("spawn", Some(n), h) => {
                let mut args = vec!["spawn".to_string(), n.to_string(), "--room".into(), self.room.clone()];
                if let Some(h) = h {
                    if h.contains('@') { args.extend(["--on".into(), h.into()]); }
                    else { args.extend(["--harness".into(), h.into()]); }
                }
                if let Some(host) = parts.next() { args.extend(["--on".into(), host.to_string()]); }
                self.orch(&args);
            }
            ("agents", h, _) => {
                let mut args = vec!["agents".to_string()];
                if let Some(h) = h { args.extend(["--on".into(), h.into()]); }
                self.orch(&args);
            }
            ("killagent", Some(n), h) => {
                let mut args = vec!["kill".to_string(), n.to_string()];
                if let Some(h) = h { args.extend(["--on".into(), h.into()]); }
                self.orch(&args);
            }
            ("restart", Some(n), h) => {
                let mut args = vec!["restart".to_string(), n.to_string()];
                if let Some(h) = h { args.extend(["--on".into(), h.into()]); }
                self.orch(&args);
            }
            ("usage", _, _) => match get(&format!("{}/usage", self.hub), &self.token) {
                Some(v) => {
                    let list: Vec<String> = v["usage"].as_array().cloned().unwrap_or_default()
                        .iter()
                        .map(|u| {
                            let t = u["tokens_5h"].as_i64().unwrap_or(0);
                            let tt = if t >= 1_000_000 { format!("{:.1}M", t as f64 / 1e6) }
                                     else { format!("{}k", t / 1000) };
                            format!("{}/{}: {tt}",
                                u["machine"].as_str().unwrap_or("?"),
                                u["harness"].as_str().unwrap_or("?"))
                        })
                        .collect();
                    self.status = if list.is_empty() { "usage: no reports yet".into() }
                                  else { format!("5h tokens — {}", list.join("  ")) };
                }
                None => self.status = "usage: fetch failed".into(),
            },
            ("kick", Some(_), _) => {
                let names: Vec<String> = cmd.split_whitespace().skip(1).map(String::from).collect();
                let payload = serde_json::json!({ "room": self.room, "names": names });
                match ureq::post(&format!("{}/kick", self.hub))
                    .header("x-agora-token", &self.token)
                    .send_json(&payload)
                {
                    Ok(_) => { self.status = format!("kicked: {}", names.join(", ")); self.refresh(); }
                    Err(e) => self.status = format!("kick failed: {e}"),
                }
            }
            ("delroom", Some(r), _) => {
                let payload = serde_json::json!({ "room": r });
                match ureq::post(&format!("{}/delroom", self.hub))
                    .header("x-agora-token", &self.token)
                    .send_json(&payload)
                {
                    Ok(_) => {
                        self.status = format!("deleted room {r}");
                        if self.room == r { self.room = "dev".into(); }
                        self.fetch_rooms();
                        self.refresh();
                    }
                    Err(e) => self.status = format!("delroom failed: {e}"),
                }
            }
            _ => self.status = "commands: /rooms /room /move /kick /delroom /name /quit".into(),
        }
    }

    /// Build bottom-up lines for the message pane and record y-ranges for
    /// click hit-testing. ponytail: newest-anchored window only, no free
    /// scrolling — add an offset when someone actually needs deep history.
    fn build_timeline(&mut self, area: Rect, accent: Color) -> Vec<Line<'static>> {
        self.msg_area = area;
        self.vis.clear();
        let width = area.width.saturating_sub(2) as usize;
        let budget = area.height as usize;
        let list = self.current_list().clone();

        let mut blocks: Vec<(usize, Vec<Line<'static>>)> = Vec::new();
        let mut used = 0usize;
        for (idx, m) in list.iter().enumerate().rev() {
            let from = m["from"].as_str().unwrap_or("?").to_string();
            let body = m["body"].as_str().unwrap_or("");
            let kind = m["kind"].as_str().unwrap_or("msg").to_string();
            let to = m["to"].as_str().map(String::from);
            let at = m["at"].as_i64().unwrap_or(0);
            let ts = format!("{:02}:{:02}", at % 86400 / 3600, at % 3600 / 60);
            let selected = self.selected == Some(idx);
            let key = self.msg_key(m);

            let mut header = vec![
                Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
                Span::styled(from, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            ];
            if let Some(t) = to {
                header.push(Span::styled(format!(" → {t}"), Style::default().fg(Color::Magenta)));
            }
            if kind != "msg" && kind != "summary" {
                header.push(Span::styled(format!(" [{kind}]"), Style::default().fg(Color::Yellow)));
            }
            if selected {
                header.insert(0, Span::styled("▶ ", Style::default().fg(Color::White)));
            }

            let wrapped = wrap_text(body, width.saturating_sub(2));
            let expanded = self.expanded.contains(&key);
            let mut lines = vec![Line::from(header)];
            if wrapped.len() > COLLAPSE_AT && !expanded {
                for l in wrapped.iter().take(COLLAPSE_AT - 1) {
                    lines.push(Line::from(Span::raw(format!("  {l}"))));
                }
                lines.push(Line::from(Span::styled(
                    format!("  … +{} more lines (click / Enter to expand)", wrapped.len() - (COLLAPSE_AT - 1)),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )));
            } else {
                for l in &wrapped {
                    lines.push(Line::from(Span::raw(format!("  {l}"))));
                }
                if wrapped.len() > COLLAPSE_AT {
                    lines.push(Line::from(Span::styled(
                        "  … (click / Enter to collapse)",
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            if used + lines.len() > budget {
                break;
            }
            used += lines.len();
            blocks.push((idx, lines));
        }
        blocks.reverse();

        let mut out: Vec<Line> = Vec::new();
        let mut y = area.y;
        for (idx, lines) in blocks {
            let h = lines.len() as u16;
            self.vis.push((idx, y, h));
            out.extend(lines);
            y += h;
        }
        out
    }

    fn build_peers(&mut self, area: Rect) -> Vec<Line<'static>> {
        self.peers_area = area;
        self.peer_vis.clear();
        self.menu_items.clear();
        let w = area.width as usize;
        // clip to panel width so nothing overflows the border
        let clip = |s: &str, reserve: usize| -> String {
            let max = w.saturating_sub(reserve + 1); // +1 = right margin before border
            if s.chars().count() <= max { s.to_string() }
            else { s.chars().take(max.saturating_sub(1)).collect::<String>() + "…" }
        };
        let mut out: Vec<Line> = Vec::new();
        let mut y = area.y;
        for (idx, p) in self.peers.iter().enumerate() {
            let name = p["name"].as_str().unwrap_or("?").to_string();
            let harness = p["harness"].as_str().unwrap_or("").to_string();
            let harness = if harness.is_empty() { "unknown".to_string() } else { harness };
            let status = p["status"].as_str().unwrap_or("").to_string();
            let idle = p["idle_secs"].as_i64().unwrap_or(0);
            let dot = if idle < 120 { Span::styled("● ", Style::default().fg(Color::Green)) }
                      else { Span::styled("○ ", Style::default().fg(Color::DarkGray)) };
            // ● + optional ▶ marker take ~2-3 cols
            let sel = self.peers_focused && self.peer_sel == idx && self.peer_menu.is_none();
            let name_reserve = if sel { 4 } else { 2 };
            let mut first = vec![dot, Span::styled(clip(&name, name_reserve), Style::default().add_modifier(Modifier::BOLD))];
            if sel {
                first.insert(0, Span::styled("▶", Style::default().fg(Color::White)));
            }
            let mut lines = vec![Line::from(first)];
            let sub = if status.is_empty() { harness } else { format!("{harness} · {status}") };
            if !sub.is_empty() {
                lines.push(Line::from(Span::styled(format!("  {}", clip(&sub, 3)), Style::default().fg(Color::DarkGray))));
            }
            let h = lines.len() as u16;
            if y + h > area.y + area.height { break; }
            self.peer_vis.push((idx, y, h));
            out.extend(lines);
            y += h;
            // context menu under the open peer
            if self.peer_menu == Some(idx) {
                let items: [(&str, MenuAction); 5] = [
                    ("  ✉ message", MenuAction::Message),
                    ("  ⤒ reveal", MenuAction::Reveal),
                    ("  ✂ kick", MenuAction::Kick),
                    ("  → move…", MenuAction::Move),
                    ("  ✕ close", MenuAction::Close),
                ];
                for (i, (label, action)) in items.into_iter().enumerate() {
                    if y >= area.y + area.height { break; }
                    let rect = Rect::new(area.x, y, area.width, 1);
                    self.menu_items.push((rect, action));
                    let bg = if self.menu_sel == i { Color::Rgb(70, 76, 94) } else { Color::Rgb(40, 44, 56) };
                    out.push(Line::from(Span::styled(
                        label.to_string(),
                        Style::default().fg(Color::Cyan).bg(bg),
                    )));
                    y += 1;
                }
            }
        }
        out
    }

    fn menu_action(&mut self, action: MenuAction) {
        let Some(idx) = self.peer_menu.take() else { return };
        let Some(name) = self.peers.get(idx).and_then(|p| p["name"].as_str()).map(String::from) else { return };
        match action {
            MenuAction::Reveal => {
                let id = self.peers.get(idx).and_then(|p| p["id"].as_i64()).unwrap_or(-1);
                let bin = format!("{}/workspace/agora/target/release/wake", std::env::var("HOME").unwrap_or_default());
                match std::process::Command::new(&bin).args(["reveal", &id.to_string()]).output() {
                    Ok(o) => self.status = String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    Err(e) => self.status = format!("reveal failed: {e}"),
                }
            }
            MenuAction::Message => {
                // append recipients: @a @b message
                let trimmed = self.input.trim_start().to_string();
                if trimmed.starts_with('@') && !self.input.contains(|c: char| c == ' ') || trimmed.split_whitespace().all(|w| w.starts_with('@')) && !trimmed.is_empty() {
                    self.input = format!("{} @{name} ", trimmed.trim_end());
                } else if trimmed.is_empty() {
                    self.input = format!("@{name} ");
                } else {
                    self.input = format!("@{name} {trimmed}");
                }
            }
            MenuAction::Move => self.input = format!("/move {name} "),
            MenuAction::Kick => self.command(format!("kick {name}")),
            MenuAction::Close => {}
        }
    }

    fn click(&mut self, col: u16, row: u16) {
        // open menu intercepts clicks first
        if self.peer_menu.is_some() {
            if let Some(&(_, action)) = self.menu_items.iter().find(|(r, _)| {
                col >= r.x && col < r.x + r.width && row == r.y
            }) {
                self.menu_action(action);
            } else {
                self.peer_menu = None;
            }
            return;
        }
        // peers panel: click opens the context menu
        let pa = self.peers_area;
        if col >= pa.x && col < pa.x + pa.width && row >= pa.y && row < pa.y + pa.height {
            if let Some(&(idx, _, _)) = self.peer_vis.iter().find(|&&(_, y, h)| row >= y && row < y + h) {
                self.peer_menu = Some(idx);
            }
            return;
        }
        // timeline: click toggles expand
        let a = self.msg_area;
        if col < a.x || col >= a.x + a.width || row < a.y || row >= a.y + a.height {
            return;
        }
        if let Some(&(idx, _, _)) = self.vis.iter().find(|&&(_, y, h)| row >= y && row < y + h) {
            self.selected = Some(idx);
            self.toggle(idx);
        }
    }

    fn select_move(&mut self, delta: i64) {
        let visible: Vec<usize> = self.vis.iter().map(|&(i, _, _)| i).collect();
        if visible.is_empty() {
            return;
        }
        let pos = self
            .selected
            .and_then(|s| visible.iter().position(|&i| i == s))
            .map(|p| (p as i64 + delta).clamp(0, visible.len() as i64 - 1) as usize)
            .unwrap_or(visible.len() - 1);
        self.selected = Some(visible[pos]);
    }
}

fn main() -> io::Result<()> {
    let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
    let room = std::env::var("AGORA_ROOM").unwrap_or_else(|_| "dev".into());
    let home_dir = std::env::var("HOME").unwrap_or_default();
    let name_file = format!("{home_dir}/.agora-name");
    let name = match std::env::var("AGORA_NAME").ok().filter(|s| !s.trim().is_empty()) {
        Some(n) => n,
        None => match std::fs::read_to_string(&name_file).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(n) => n,
            None => {
                // first run: ask the user to pick a personal name
                use std::io::Write;
                let default = format!("{}-tui", std::env::var("USER").unwrap_or_else(|_| "user".into()));
                print!("Welcome to agora. Pick a display name [{default}]: ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                let chosen = if std::io::stdin().read_line(&mut line).is_ok() {
                    let t = line.trim();
                    if t.is_empty() { default } else { t.to_string() }
                } else {
                    default
                };
                let _ = std::fs::write(&name_file, &chosen);
                chosen
            }
        },
    };
    let token = std::env::var("AGORA_INGEST_TOKEN").ok().or_else(|| {
        let home = std::env::var("HOME").ok()?;
        std::fs::read_to_string(format!("{home}/.agora-ingest-token")).ok()
    });
    let Some(token) = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
        eprintln!("no token: set AGORA_INGEST_TOKEN or ~/.agora-ingest-token");
        std::process::exit(1);
    };

    let mut app = App {
        hub, room, name, token,
        messages: vec![], feed: vec![], peers: vec![],
        input: String::new(), status: String::new(), show_feed: false,
        expanded: HashSet::new(), selected: None, vis: vec![], msg_area: Rect::default(),
        suggest_idx: 0, quit: false,
        rooms: vec![], peer_vis: vec![], peers_area: Rect::default(),
        peer_menu: None, menu_items: vec![],
        peers_focused: false, peer_sel: 0, menu_sel: 0,
    };
    app.fetch_rooms();
    app.refresh();
    app.status = format!(
        "room {} · {} msgs · {} feed · Tab=feed ↑↓=select Enter=send/expand /help=cmds q=quit",
        app.room, app.messages.len(), app.feed.len()
    );

    let mut terminal = ratatui::init();
    crossterm::execute!(io::stdout(), event::EnableMouseCapture).ok();
    let mut last_poll = Instant::now();
    let res = loop {
        let gold = Color::Rgb(227, 179, 65);
        // precompute layout so build_timeline can record hit-test ranges
        let size = terminal.size()?;
        let full = Rect::new(0, 0, size.width, size.height);
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)])
            .split(full);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(66), Constraint::Percentage(34)])
            .split(outer[0]);
        let inner = Rect::new(cols[0].x + 1, cols[0].y + 1, cols[0].width.saturating_sub(2), cols[0].height.saturating_sub(2));
        let accent = if app.show_feed { Color::Cyan } else { gold };
        let timeline = app.build_timeline(inner, accent);
        let peers_inner = Rect::new(cols[1].x + 1, cols[1].y + 1, cols[1].width.saturating_sub(2), cols[1].height.saturating_sub(2));
        let peer_pane = app.build_peers(peers_inner);
        let main_title = if app.show_feed { " feed (ambient) " } else { " messages " };

        terminal.draw(|f| {
            f.render_widget(
                Paragraph::new(timeline.clone()).block(
                    Block::default().borders(Borders::ALL).title(main_title)
                        .border_style(Style::default().fg(if app.peers_focused { Color::DarkGray } else { gold })),
                ),
                cols[0],
            );

            f.render_widget(
                Block::default().borders(Borders::ALL).title(" peers ")
                    .border_style(Style::default().fg(if app.peers_focused { gold } else { Color::DarkGray })),
                cols[1],
            );
            f.render_widget(Paragraph::new(peer_pane.clone()), peers_inner);

            f.render_widget(
                Paragraph::new(app.input.as_str()).wrap(Wrap { trim: false }).block(
                    Block::default().borders(Borders::ALL)
                        .title(format!(" [{}] post as {} (@name=DM /help=cmds) ", app.room, app.name))
                        .border_style(Style::default().fg(gold)),
                ),
                outer[1],
            );
            f.render_widget(
                Paragraph::new(Span::styled(app.status.as_str(), Style::default().fg(Color::DarkGray))),
                outer[2],
            );
            // Claude Code-style slash popup above the input
            let sugg = app.suggestions();
            if !sugg.is_empty() {
                let h = sugg.len() as u16;
                let w = outer[1].width.min(72);
                let area = Rect::new(outer[1].x, outer[1].y.saturating_sub(h), w, h);
                f.render_widget(ratatui::widgets::Clear, area);
                let rows: Vec<Line> = sugg.iter().enumerate().map(|(i, s)| {
                    let selected = i == app.suggest_idx.min(sugg.len() - 1);
                    let row_style = if selected {
                        Style::default().bg(Color::Rgb(40, 44, 56))
                    } else {
                        Style::default()
                    };
                    Line::from(vec![
                        Span::styled(s.label.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD).patch(row_style)),
                        Span::styled(" ".repeat(26usize.saturating_sub(s.label.chars().count())), row_style),
                        Span::styled(s.desc.clone(), Style::default().fg(Color::Gray).patch(row_style)),
                    ]).style(row_style)
                }).collect();
                f.render_widget(Paragraph::new(rows), area);
            }
            f.set_cursor_position((
                outer[1].x + 1 + app.input.chars().count() as u16,
                outer[1].y + 1,
            ));
        })?;

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(k) => {
                    let popup = !app.suggestions().is_empty();
                    let menu_open = app.peer_menu.is_some();
                    match (k.code, k.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Ok(()),
                        (KeyCode::Char('q'), m) if app.input.is_empty() && m.is_empty() => break Ok(()),
                        (KeyCode::Left, _) | (KeyCode::Right, _) if !popup => {
                            app.peers_focused = !app.peers_focused;
                            app.peer_menu = None;
                        }
                        (KeyCode::Tab, _) if popup => { app.complete_suggestion(); }
                        (KeyCode::Tab, _) => { app.show_feed = !app.show_feed; app.selected = None; }
                        (KeyCode::Enter, _) if menu_open => {
                            let action = app.menu_items.get(app.menu_sel).map(|&(_, a)| a);
                            if let Some(a) = action { app.menu_action(a); }
                        }
                        (KeyCode::Enter, _) if popup => { app.complete_suggestion(); }
                        (KeyCode::Enter, _) if app.peers_focused => {
                            app.peer_menu = Some(app.peer_sel.min(app.peers.len().saturating_sub(1)));
                            app.menu_sel = 0;
                        }
                        (KeyCode::Enter, _) => app.send(),
                        (KeyCode::Up, _) if menu_open => app.menu_sel = app.menu_sel.saturating_sub(1),
                        (KeyCode::Down, _) if menu_open => app.menu_sel = (app.menu_sel + 1).min(4),
                        (KeyCode::Up, _) if popup => app.suggest_idx = app.suggest_idx.saturating_sub(1),
                        (KeyCode::Down, _) if popup => {
                            app.suggest_idx = (app.suggest_idx + 1).min(app.suggestions().len().saturating_sub(1));
                        }
                        (KeyCode::Up, _) if app.peers_focused => app.peer_sel = app.peer_sel.saturating_sub(1),
                        (KeyCode::Down, _) if app.peers_focused => {
                            app.peer_sel = (app.peer_sel + 1).min(app.peers.len().saturating_sub(1));
                        }
                        (KeyCode::Up, _) => app.select_move(-1),
                        (KeyCode::Down, _) => app.select_move(1),
                        (KeyCode::Esc, _) => {
                            if app.peer_menu.is_some() { app.peer_menu = None; }
                            else if app.peers_focused { app.peers_focused = false; }
                            else if app.input.is_empty() { app.selected = None; }
                            else { app.input.clear(); }
                        }
                        (KeyCode::Backspace, _) => { app.input.pop(); app.suggest_idx = 0; }
                        (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => {
                            app.input.push(c);
                            app.suggest_idx = 0;
                        }
                        _ => {}
                    }
                    if app.quit { break Ok(()); }
                },
                Event::Mouse(me) => {
                    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                        app.click(me.column, me.row);
                    }
                }
                _ => {}
            }
        }
        if last_poll.elapsed() >= Duration::from_secs(2) {
            app.refresh();
            last_poll = Instant::now();
        }
    };
    crossterm::execute!(io::stdout(), event::DisableMouseCapture).ok();
    ratatui::restore();
    res
}

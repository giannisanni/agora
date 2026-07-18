//! agora TUI: live room command center.
//! Left: message timeline (collapsed to 5 lines; click or ↑/↓ + Enter to
//! expand). Right: peers. Bottom: input.
//! Keys: Tab feed/messages, Enter send (or toggle expand when input empty and
//! a message is selected), ↑/↓ select, Esc clear, q / Ctrl-C quit.
//! Slash commands: /rooms /room <name> /move <agent> <room> /name <me>
//! Env: AGORA_HUB, AGORA_ROOM, AGORA_NAME; token from ~/.agora-ingest-token.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

const COLLAPSE_AT: usize = 5; // bodies longer than this collapse to 4 + hint

// (name, args hint, description) — powers the / autocomplete popup
const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "Show available commands"),
    ("rooms", "", "List all rooms with agent/message counts"),
    ("room", "<name>", "Switch to a room (creates it on first post)"),
    ("move", "<agent> <room>", "Move an agent from this room to another"),
    ("name", "<me>", "Change the name you post as"),
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

    fn refresh(&mut self) {
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
        let (to, body) = match text.strip_prefix('@') {
            Some(rest) => match rest.split_once(' ') {
                Some((name, msg)) => (Some(name.to_string()), msg.to_string()),
                None => (None, text.clone()),
            },
            None => (None, text.clone()),
        };
        let payload = serde_json::json!({
            "room": self.room, "name": self.name, "harness": "human-tui",
            "machine": "tui", "body": body, "kind": "msg", "to": to,
        });
        match ureq::post(&format!("{}/ingest", self.hub))
            .header("x-agora-token", &self.token)
            .send_json(&payload)
        {
            Ok(_) => {
                self.status = format!("sent{}", to.map(|t| format!(" → {t}")).unwrap_or_default());
                self.input.clear();
                self.refresh();
            }
            Err(e) => self.status = format!("send failed: {e}"),
        }
    }

    /// Commands matching the partial "/xyz" being typed (popup contents).
    fn suggestions(&self) -> Vec<&'static (&'static str, &'static str, &'static str)> {
        let Some(rest) = self.input.strip_prefix('/') else { return vec![] };
        if rest.contains(' ') {
            return vec![];
        }
        COMMANDS.iter().filter(|(n, _, _)| n.starts_with(rest)).collect()
    }

    fn complete_suggestion(&mut self) -> bool {
        let sugg = self.suggestions();
        let Some(&&(name, args, _)) = sugg.get(self.suggest_idx.min(sugg.len().saturating_sub(1))) else {
            return false;
        };
        if args.is_empty() {
            // no-arg command: run immediately
            self.input.clear();
            self.command(name.to_string());
        } else {
            self.input = format!("/{name} ");
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
                self.name = n.to_string();
                self.status = format!("posting as {}", self.name);
            }
            ("quit", _, _) => self.quit = true,
            _ => self.status = "commands: /rooms  /room <name>  /move <agent> <room>  /name <me>".into(),
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

    fn click(&mut self, col: u16, row: u16) {
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
    let name = std::env::var("AGORA_NAME").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        format!("{user}-tui")
    });
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
    };
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
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(outer[0]);
        let inner = Rect::new(cols[0].x + 1, cols[0].y + 1, cols[0].width.saturating_sub(2), cols[0].height.saturating_sub(2));
        let accent = if app.show_feed { Color::Cyan } else { gold };
        let timeline = app.build_timeline(inner, accent);
        let main_title = if app.show_feed { " feed (ambient) " } else { " messages " };

        terminal.draw(|f| {
            f.render_widget(
                Paragraph::new(timeline.clone()).block(
                    Block::default().borders(Borders::ALL).title(main_title)
                        .border_style(Style::default().fg(gold)),
                ),
                cols[0],
            );

            let peer_lines: Vec<ListItem> = app.peers.iter().map(|p| {
                let name = p["name"].as_str().unwrap_or("?");
                let harness = p["harness"].as_str().unwrap_or("");
                let status = p["status"].as_str().unwrap_or("");
                let idle = p["idle_secs"].as_i64().unwrap_or(0);
                let dot = if idle < 120 { Span::styled("● ", Style::default().fg(Color::Green)) }
                          else { Span::styled("○ ", Style::default().fg(Color::DarkGray)) };
                let mut lines = vec![Line::from(vec![
                    dot,
                    Span::styled(name.to_string(), Style::default().add_modifier(Modifier::BOLD)),
                ])];
                let sub = if status.is_empty() { harness.to_string() } else { format!("{harness} · {status}") };
                if !sub.is_empty() {
                    lines.push(Line::from(Span::styled(format!("  {sub}"), Style::default().fg(Color::DarkGray))));
                }
                ListItem::new(lines)
            }).collect();
            f.render_widget(
                List::new(peer_lines).block(
                    Block::default().borders(Borders::ALL).title(" peers ")
                        .border_style(Style::default().fg(Color::DarkGray)),
                ),
                cols[1],
            );

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
                let typed = app.input.trim_start_matches('/');
                let rows: Vec<Line> = sugg.iter().enumerate().map(|(i, (n, args, desc))| {
                    let selected = i == app.suggest_idx.min(sugg.len() - 1);
                    let row_style = if selected {
                        Style::default().bg(Color::Rgb(40, 44, 56))
                    } else {
                        Style::default()
                    };
                    let cmd_col = format!("/{n} {args}");
                    let mut spans = vec![
                        Span::styled(format!("/{typed}"), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::styled(n[typed.len()..].to_string(), Style::default().fg(Color::Cyan)),
                        Span::styled(format!(" {args}"), Style::default().fg(Color::DarkGray)),
                        Span::raw(" ".repeat(24usize.saturating_sub(cmd_col.len()))),
                        Span::styled((*desc).to_string(), Style::default().fg(Color::Gray)),
                    ];
                    for s in &mut spans {
                        s.style = s.style.patch(row_style);
                    }
                    Line::from(spans).style(row_style)
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
                    match (k.code, k.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Ok(()),
                        (KeyCode::Char('q'), m) if app.input.is_empty() && m.is_empty() => break Ok(()),
                        (KeyCode::Tab, _) if popup => { app.complete_suggestion(); }
                        (KeyCode::Tab, _) => { app.show_feed = !app.show_feed; app.selected = None; }
                        (KeyCode::Enter, _) if popup => { app.complete_suggestion(); }
                        (KeyCode::Enter, _) => app.send(),
                        (KeyCode::Up, _) if popup => {
                            app.suggest_idx = app.suggest_idx.saturating_sub(1);
                        }
                        (KeyCode::Down, _) if popup => {
                            app.suggest_idx = (app.suggest_idx + 1).min(app.suggestions().len().saturating_sub(1));
                        }
                        (KeyCode::Up, _) => app.select_move(-1),
                        (KeyCode::Down, _) => app.select_move(1),
                        (KeyCode::Esc, _) => {
                            if app.input.is_empty() { app.selected = None; } else { app.input.clear(); }
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

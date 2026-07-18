//! agora TUI: live room viewer + post box.
//! Left: message timeline. Right: feed (ambient) + peers. Bottom: input.
//! Keys: Tab switch timeline/feed focus, Enter send, Esc clear, q / Ctrl-C quit.
//! Env: AGORA_HUB, AGORA_ROOM, AGORA_NAME (default gianni-tui), token from
//! ~/.agora-ingest-token or AGORA_INGEST_TOKEN.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

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

impl App {
    fn refresh(&mut self) {
        let base = format!("{}?room={}", "", self.room); // placeholder to keep fmt simple
        let _ = base;
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

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        // "@name message" = targeted
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
}

fn ts(unix: i64) -> String {
    // hh:mm local from unix secs; no chrono, coarse TZ via env-free libc localtime
    // ponytail: UTC display; local-TZ needs chrono
    let secs = unix % 86400;
    format!("{:02}:{:02}", secs / 3600 % 24, secs / 60 % 60)
}

fn msg_lines<'a>(msgs: &'a [serde_json::Value], accent: Color) -> Vec<ListItem<'a>> {
    msgs.iter()
        .map(|m| {
            let from = m["from"].as_str().unwrap_or("?");
            let body = m["body"].as_str().unwrap_or("");
            let kind = m["kind"].as_str().unwrap_or("msg");
            let to = m["to"].as_str();
            let at = ts(m["at"].as_i64().unwrap_or(0));
            let mut header = vec![
                Span::styled(format!("{at} "), Style::default().fg(Color::DarkGray)),
                Span::styled(from.to_string(), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            ];
            if let Some(t) = to {
                header.push(Span::styled(format!(" → {t}"), Style::default().fg(Color::Magenta)));
            }
            if kind != "msg" && kind != "summary" {
                header.push(Span::styled(format!(" [{kind}]"), Style::default().fg(Color::Yellow)));
            }
            let mut lines = vec![Line::from(header)];
            for l in body.lines().take(6) {
                lines.push(Line::from(Span::raw(format!("  {l}"))));
            }
            ListItem::new(lines)
        })
        .collect()
}

fn main() -> io::Result<()> {
    let hub = std::env::var("AGORA_HUB").unwrap_or_else(|_| "http://100.84.87.107:8787".into());
    let room = std::env::var("AGORA_ROOM").unwrap_or_else(|_| "dev".into());
    let name = std::env::var("AGORA_NAME").unwrap_or_else(|_| "gianni-tui".into());
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
        input: String::new(), status: "connecting...".into(), show_feed: false,
    };
    app.refresh();
    app.status = format!("room {} · {} msgs · {} feed · Tab=feed Enter=send @name=DM q=quit", app.room, app.messages.len(), app.feed.len());

    let mut terminal = ratatui::init();
    let mut last_poll = Instant::now();
    let res = loop {
        terminal.draw(|f| {
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(3), Constraint::Length(1)])
                .split(f.area());
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
                .split(outer[0]);

            let gold = Color::Rgb(227, 179, 65);
            let (main_title, main_items, main_accent) = if app.show_feed {
                (" feed (ambient) ", &app.feed, Color::Cyan)
            } else {
                (" messages ", &app.messages, gold)
            };
            let items = msg_lines(main_items, main_accent);
            let count = items.len();
            let list = List::new(items).block(
                Block::default().borders(Borders::ALL).title(main_title)
                    .border_style(Style::default().fg(gold)),
            );
            let mut state = ratatui::widgets::ListState::default();
            if count > 0 { state.select(Some(count - 1)); }
            f.render_stateful_widget(list, cols[0], &mut state);

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
                    Span::styled(format!(" {harness}"), Style::default().fg(Color::DarkGray)),
                ])];
                if !status.is_empty() {
                    lines.push(Line::from(Span::styled(format!("   {status}"), Style::default().fg(Color::Gray))));
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
                        .title(format!(" post as {} (@name for DM) ", app.name))
                        .border_style(Style::default().fg(gold)),
                ),
                outer[1],
            );
            f.render_widget(
                Paragraph::new(Span::styled(app.status.as_str(), Style::default().fg(Color::DarkGray))),
                outer[2],
            );
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Ok(()),
                    (KeyCode::Char('q'), m) if app.input.is_empty() && m.is_empty() => break Ok(()),
                    (KeyCode::Tab, _) => app.show_feed = !app.show_feed,
                    (KeyCode::Enter, _) => app.send(),
                    (KeyCode::Esc, _) => app.input.clear(),
                    (KeyCode::Backspace, _) => { app.input.pop(); }
                    (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => app.input.push(c),
                    _ => {}
                }
            }
        }
        if last_poll.elapsed() >= Duration::from_secs(2) {
            app.refresh();
            last_poll = Instant::now();
        }
    };
    ratatui::restore();
    res
}

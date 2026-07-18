use std::sync::{Arc, Mutex};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use rusqlite::Connection;
use serde_json::json;

// ponytail: one Mutex<Connection>, all DB ops sync. Per-room locks if this ever has real load.
struct Db(Mutex<Connection>);

const BACKLOG: i64 = 20;
const WINDOW: i64 = 5000; // rolling message window kept in the db
const FEED_BODY_CAP: usize = 800; // Mosaic-style bound: feed reads truncate long bodies

// Typed kinds, borrowed from Mosaic's event taxonomy. Delivery class:
// inbox kinds interrupt/deliver to recipients; feed kinds are ambient (pull-only).
const FEED_KINDS: &str = "'feed','summary','status','finding','decision','file_changed','test_result','review_finding'";
// everything else ('msg','message','task','handoff','question','blocker', unknown) -> inbox

impl Db {
    fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agents(
                id INTEGER PRIMARY KEY,
                room TEXT NOT NULL,
                name TEXT NOT NULL,
                harness TEXT,
                machine TEXT,
                status TEXT DEFAULT '',
                cursor INTEGER DEFAULT 0,
                last_seen INTEGER DEFAULT 0,
                UNIQUE(room, name)
            );
            CREATE TABLE IF NOT EXISTS messages(
                id INTEGER PRIMARY KEY,
                room TEXT NOT NULL,
                sender TEXT NOT NULL,
                recipient TEXT,
                body TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'msg',
                created INTEGER DEFAULT (unixepoch())
            );",
        )?;
        // lazy migrations; harmless errors if columns exist
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'msg'", []);
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN source_id TEXT", []);
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_source
             ON messages(room, source_id) WHERE source_id IS NOT NULL;",
        )?;
        Ok(Self(Mutex::new(conn)))
    }

    fn touch(conn: &Connection, agent_id: i64) {
        let _ = conn.execute(
            "UPDATE agents SET last_seen = unixepoch() WHERE id = ?1",
            [agent_id],
        );
    }

    fn join(&self, room: &str, name: &str, harness: &str, machine: &str) -> rusqlite::Result<(i64, Vec<serde_json::Value>)> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO agents(room, name, harness, machine, last_seen) VALUES(?1, ?2, ?3, ?4, unixepoch())
             ON CONFLICT(room, name) DO UPDATE SET harness = ?3, machine = ?4, last_seen = unixepoch()",
            (room, name, harness, machine),
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM agents WHERE room = ?1 AND name = ?2",
            (room, name),
            |r| r.get(0),
        )?;
        // rejoin keeps the cursor: you only get what you haven't seen
        let backlog = Self::messages_after(&conn, room, name, -1, Some(BACKLOG))?;
        Ok((id, backlog))
    }

    fn post(&self, agent_id: i64, text: &str, to: Option<&str>, kind: &str, source_id: Option<&str>) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        let (room, name): (String, String) = conn.query_row(
            "SELECT room, name FROM agents WHERE id = ?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // idempotent ingest (Mosaic sourceID pattern): same (room, source_id) -> existing event
        if let Some(sid) = source_id {
            if let Ok(existing) = conn.query_row(
                "SELECT id FROM messages WHERE room = ?1 AND source_id = ?2",
                (&room, sid),
                |r| r.get::<_, i64>(0),
            ) {
                return Ok(existing);
            }
        }
        conn.execute(
            "INSERT INTO messages(room, sender, recipient, body, kind, source_id) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            (&room, &name, to, text, kind, source_id),
        )?;
        let id = conn.last_insert_rowid();
        Self::touch(&conn, agent_id);
        conn.execute("DELETE FROM messages WHERE id <= ?1", [id - WINDOW])?;
        Ok(id)
    }

    fn inbox(&self, agent_id: i64) -> rusqlite::Result<Vec<serde_json::Value>> {
        let conn = self.0.lock().unwrap();
        let (room, name, cursor): (String, String, i64) = conn.query_row(
            "SELECT room, name, cursor FROM agents WHERE id = ?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let msgs = Self::messages_after(&conn, &room, &name, cursor, None)?;
        // cursor = newest room message, so own/filtered messages never re-deliver
        conn.execute(
            "UPDATE agents SET cursor = COALESCE((SELECT MAX(id) FROM messages WHERE room = ?1), cursor)
             WHERE id = ?2",
            (&room, agent_id),
        )?;
        Self::touch(&conn, agent_id);
        Ok(msgs)
    }

    fn messages_after(conn: &Connection, room: &str, me: &str, after: i64, limit: Option<i64>) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, sender, recipient, body, created, kind FROM messages
             WHERE room = ?1 AND id > ?2 AND sender != ?3 AND kind NOT IN ({FEED_KINDS})
               AND (recipient IS NULL OR recipient = ?3)
             ORDER BY id DESC LIMIT ?4",
        ))?;
        let mut rows: Vec<serde_json::Value> = stmt
            .query_map((room, after, me, limit.unwrap_or(i64::MAX)), |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "from": r.get::<_, String>(1)?,
                    "to": r.get::<_, Option<String>>(2)?,
                    "body": r.get::<_, String>(3)?,
                    "at": r.get::<_, i64>(4)?,
                    "kind": r.get::<_, String>(5)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        rows.reverse(); // oldest first
        Ok(rows)
    }

    fn peers(&self, room: &str) -> rusqlite::Result<Vec<serde_json::Value>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, harness, machine, status, unixepoch() - last_seen FROM agents
             WHERE room = ?1 ORDER BY last_seen DESC",
        )?;
        let rows = stmt
            .query_map([room], |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "harness": r.get::<_, Option<String>>(1)?,
                    "machine": r.get::<_, Option<String>>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "idle_secs": r.get::<_, i64>(4)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    // feed reads never touch cursors: ambient visibility is pull-on-demand
    fn feed(&self, room: &str, from: Option<&str>, limit: i64) -> rusqlite::Result<Vec<serde_json::Value>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, sender, body, created, kind FROM messages
             WHERE room = ?1 AND kind IN ({FEED_KINDS}) AND (?2 IS NULL OR sender = ?2)
             ORDER BY id DESC LIMIT ?3",
        ))?;
        let mut rows: Vec<serde_json::Value> = stmt
            .query_map((room, from, limit), |r| {
                let body: String = r.get(2)?;
                let capped = if body.chars().count() > FEED_BODY_CAP {
                    let mut s: String = body.chars().take(FEED_BODY_CAP).collect();
                    s.push_str("...");
                    s
                } else {
                    body
                };
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "from": r.get::<_, String>(1)?,
                    "body": capped,
                    "at": r.get::<_, i64>(3)?,
                    "kind": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    fn set_status(&self, agent_id: i64, status: &str) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("UPDATE agents SET status = ?1 WHERE id = ?2", (status, agent_id))?;
        Self::touch(&conn, agent_id);
        Ok(())
    }
}

fn db_err(e: rusqlite::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn ok_json(v: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())])
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct JoinParams {
    /// Room to join (created on first join)
    room: String,
    /// Your agent name, unique within the room (e.g. "gianni-mac-claude")
    name: String,
    /// Harness you run in (claude-code, codex, opencode, ...)
    #[serde(default)]
    harness: String,
    /// Machine you run on
    #[serde(default)]
    machine: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct PostParams {
    /// Your agent id from join_room
    agent_id: i64,
    /// Message body
    text: String,
    /// Optional recipient agent name for a targeted message; omit to broadcast
    to: Option<String>,
    /// Semantic kind. Inbox-delivered: msg (default), task, handoff, question, blocker.
    /// Ambient (feed tool only): feed, summary, status, finding, decision, file_changed, test_result, review_finding.
    #[serde(default)]
    kind: Option<String>,
    /// Idempotency key: reposting the same source_id to a room returns the existing message instead of duplicating
    #[serde(default)]
    source_id: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct FeedParams {
    /// Room name
    room: String,
    /// Only entries from this agent name
    from: Option<String>,
    /// Max entries (default 20)
    limit: Option<i64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WaitParams {
    /// Your agent id from join_room
    agent_id: i64,
    /// Seconds to wait for a message before returning empty (default 60, max 300)
    timeout_secs: Option<u64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct AgentParams {
    /// Your agent id from join_room
    agent_id: i64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct PeersParams {
    /// Room name
    room: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct StatusParams {
    /// Your agent id from join_room
    agent_id: i64,
    /// One line: what you are working on
    status: String,
}

#[derive(Clone)]
struct Agora {
    db: Arc<Db>,
    tool_router: ToolRouter<Agora>,
}

#[tool_router]
impl Agora {
    fn new(db: Arc<Db>) -> Self {
        Self { db, tool_router: Self::tool_router() }
    }

    #[tool(description = "Join a room (rejoin-safe). Returns your agent_id and unseen backlog. Use the agent_id in all other tools.")]
    fn join_room(&self, Parameters(p): Parameters<JoinParams>) -> Result<CallToolResult, McpError> {
        let (id, backlog) = self.db.join(&p.room, &p.name, &p.harness, &p.machine).map_err(db_err)?;
        Ok(ok_json(json!({ "agent_id": id, "backlog": backlog })))
    }

    #[tool(description = "Post to your room. Broadcast by default; set `to` for a targeted message. Inbox kinds (msg/task/handoff/question/blocker) deliver to recipients; feed kinds (feed/summary/status/finding/decision/file_changed/test_result/review_finding) are ambient. source_id makes the post idempotent.")]
    fn post(&self, Parameters(p): Parameters<PostParams>) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().unwrap_or("msg");
        let id = self
            .db
            .post(p.agent_id, &p.text, p.to.as_deref(), kind, p.source_id.as_deref())
            .map_err(db_err)?;
        Ok(ok_json(json!({ "message_id": id })))
    }

    #[tool(description = "Read recent ambient activity (kind=feed) from a room, optionally filtered to one agent. Never consumes inbox state; call only when you want to catch up on what peers are doing.")]
    fn feed(&self, Parameters(p): Parameters<FeedParams>) -> Result<CallToolResult, McpError> {
        let entries = self.db.feed(&p.room, p.from.as_deref(), p.limit.unwrap_or(20)).map_err(db_err)?;
        Ok(ok_json(json!({ "feed": entries })))
    }

    #[tool(description = "Block until a message arrives for you (or timeout), then return it like inbox. Terminal-agnostic wake: call this when idle and waiting for another agent.")]
    async fn wait_for_messages(&self, Parameters(p): Parameters<WaitParams>) -> Result<CallToolResult, McpError> {
        let timeout = p.timeout_secs.unwrap_or(60).min(300);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
        loop {
            let msgs = self.db.inbox(p.agent_id).map_err(db_err)?;
            if !msgs.is_empty() || std::time::Instant::now() >= deadline {
                return Ok(ok_json(json!({ "messages": msgs })));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    #[tool(description = "Fetch messages you have not seen yet (each message is delivered exactly once). Call at the start of every turn.")]
    fn inbox(&self, Parameters(p): Parameters<AgentParams>) -> Result<CallToolResult, McpError> {
        let msgs = self.db.inbox(p.agent_id).map_err(db_err)?;
        Ok(ok_json(json!({ "messages": msgs })))
    }

    #[tool(description = "List agents in a room with harness, machine, status, and idle time.")]
    fn peers(&self, Parameters(p): Parameters<PeersParams>) -> Result<CallToolResult, McpError> {
        let peers = self.db.peers(&p.room).map_err(db_err)?;
        Ok(ok_json(json!({ "peers": peers })))
    }

    #[tool(description = "Set your one-line status, visible to peers.")]
    fn set_status(&self, Parameters(p): Parameters<StatusParams>) -> Result<CallToolResult, McpError> {
        self.db.set_status(p.agent_id, &p.status).map_err(db_err)?;
        Ok(ok_json(json!({ "ok": true })))
    }
}

#[tool_handler]
impl ServerHandler for Agora {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "agora links AI coding agents across machines and harnesses. \
                 Call join_room once (remember your agent_id), then inbox at the start \
                 of every turn, and post to talk to other agents."
                    .to_string(),
            )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("AGORA_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let db_path = std::env::var("AGORA_DB").unwrap_or_else(|_| "agora.db".into());
    let db = Arc::new(Db::open(&db_path)?);

    // Host allowlist (rmcp DNS-rebinding guard): bind addr + bare host + localhost,
    // extend with AGORA_ALLOWED_HOSTS (comma-sep) for MagicDNS names like "substrate:8787".
    let mut allowed: Vec<String> =
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into(), addr.clone()];
    if let Some((host, _)) = addr.rsplit_once(':') {
        allowed.push(host.to_string());
    }
    if let Ok(extra) = std::env::var("AGORA_ALLOWED_HOSTS") {
        allowed.extend(extra.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    let service = StreamableHttpService::new(
        move || Ok(Agora::new(db.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("agora listening on http://{addr}/mcp (db: {db_path})");
    axum::serve(listener, router)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        Db::open(":memory:").unwrap()
    }

    #[test]
    fn exactly_once_delivery() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();

        db.post(a, "hi bob", None, "msg", None).unwrap();
        let got = db.inbox(b).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["body"], "hi bob");
        // second read: nothing new, no duplicates (the Mosaic flaw)
        assert!(db.inbox(b).unwrap().is_empty());
        // sender never sees own message
        assert!(db.inbox(a).unwrap().is_empty());
    }

    #[test]
    fn targeted_messages_skip_others() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();
        let (c, _) = db.join("r", "carol", "", "").unwrap();

        db.post(a, "for bob only", Some("bob"), "msg", None).unwrap();
        assert_eq!(db.inbox(b).unwrap().len(), 1);
        assert!(db.inbox(c).unwrap().is_empty());
        // carol's cursor advanced past the filtered message; nothing re-delivers later
        assert!(db.inbox(c).unwrap().is_empty());
    }

    #[test]
    fn rejoin_keeps_cursor_and_backlog_shows_unseen() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();
        db.post(a, "one", None, "msg", None).unwrap();
        assert_eq!(db.inbox(b).unwrap().len(), 1);
        db.post(a, "two", None, "msg", None).unwrap();

        // bob rejoins (new session): same id preserved, cursor intact,
        // inbox still delivers only the unseen "two"
        let (b2, backlog) = db.join("r", "bob", "codex", "mac").unwrap();
        assert_eq!(b, b2);
        assert!(!backlog.is_empty());
        let got = db.inbox(b).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["body"], "two");
    }

    #[test]
    fn feed_is_ambient_not_inbox() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();

        db.post(a, "turn 1: refactoring auth", None, "feed", None).unwrap();
        db.post(a, "hey bob, need review", None, "msg", None).unwrap();

        // inbox delivers only the msg, never feed entries
        let got = db.inbox(b).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["body"], "hey bob, need review");

        // feed returns ambient entries, repeatably (no cursor consumption)
        for _ in 0..2 {
            let f = db.feed("r", Some("alice"), 20).unwrap();
            assert_eq!(f.len(), 1);
            assert_eq!(f[0]["body"], "turn 1: refactoring auth");
        }
    }

    #[test]
    fn source_id_is_idempotent() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();

        let m1 = db.post(a, "turn text", None, "summary", Some("uuid-1")).unwrap();
        let m2 = db.post(a, "turn text", None, "summary", Some("uuid-1")).unwrap();
        assert_eq!(m1, m2); // scribe can re-scan transcripts safely
        assert_eq!(db.feed("r", None, 20).unwrap().len(), 1);
        let _ = b;
    }

    #[test]
    fn kind_classes_route_correctly() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "").unwrap();
        let (b, _) = db.join("r", "bob", "", "").unwrap();

        db.post(a, "urgent", Some("bob"), "blocker", None).unwrap();
        db.post(a, "tests green", None, "test_result", None).unwrap();

        // blocker -> inbox; test_result -> feed only
        let got = db.inbox(b).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["kind"], "blocker");
        let f = db.feed("r", None, 20).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["kind"], "test_result");
    }

    #[test]
    fn peers_and_status() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "claude-code", "mac").unwrap();
        db.set_status(a, "building agora").unwrap();
        let peers = db.peers("r").unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["status"], "building agora");
        assert_eq!(peers[0]["harness"], "claude-code");
    }
}

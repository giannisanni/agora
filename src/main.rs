use std::sync::{Arc, Mutex};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use rusqlite::Connection;
use serde_json::json;

type Parts = axum::http::request::Parts;

#[derive(Debug)]
enum AgoraErr {
    Db(rusqlite::Error),
    Denied,
}

impl From<rusqlite::Error> for AgoraErr {
    fn from(e: rusqlite::Error) -> Self {
        AgoraErr::Db(e)
    }
}

/// Caller identity: the Tailscale-User-Login header stamped by `tailscale
/// serve` for proxied traffic. Direct tailnet connections (no header) are the
/// owner — the direct port must be ACL-restricted to the owner's own devices.
fn caller(parts: &Parts) -> String {
    parts
        .headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("owner")
        .to_string()
}

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
        let _ = conn.execute("ALTER TABLE agents ADD COLUMN user TEXT NOT NULL DEFAULT 'owner'", []);
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

    fn join(&self, room: &str, name: &str, harness: &str, machine: &str, user: &str) -> Result<(i64, Vec<serde_json::Value>), AgoraErr> {
        let conn = self.0.lock().unwrap();
        // a room name belongs to whoever first claimed it
        let existing: Option<String> = conn
            .query_row(
                "SELECT user FROM agents WHERE room = ?1 AND name = ?2",
                (room, name),
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        if existing.is_some_and(|u| u != user) {
            return Err(AgoraErr::Denied);
        }
        conn.execute(
            "INSERT INTO agents(room, name, harness, machine, user, last_seen) VALUES(?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(room, name) DO UPDATE SET harness = ?3, machine = ?4, last_seen = unixepoch()",
            (room, name, harness, machine, user),
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

    /// The caller may act as this agent only if they created it.
    fn verify(conn: &Connection, agent_id: i64, user: &str) -> Result<(), AgoraErr> {
        let owner: String = conn
            .query_row("SELECT user FROM agents WHERE id = ?1", [agent_id], |r| r.get(0))
            .map_err(|e| if e == rusqlite::Error::QueryReturnedNoRows { AgoraErr::Denied } else { e.into() })?;
        if owner != user { Err(AgoraErr::Denied) } else { Ok(()) }
    }

    fn post(&self, agent_id: i64, user: &str, text: &str, to: Option<&str>, kind: &str, source_id: Option<&str>) -> Result<(i64, bool), AgoraErr> {
        let conn = self.0.lock().unwrap();
        Self::verify(&conn, agent_id, user)?;
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
                return Ok((existing, false));
            }
        }
        conn.execute(
            "INSERT INTO messages(room, sender, recipient, body, kind, source_id) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            (&room, &name, to, text, kind, source_id),
        )?;
        let id = conn.last_insert_rowid();
        Self::touch(&conn, agent_id);
        conn.execute("DELETE FROM messages WHERE id <= ?1", [id - WINDOW])?;
        Ok((id, true))
    }

    fn inbox(&self, agent_id: i64, user: &str) -> Result<Vec<serde_json::Value>, AgoraErr> {
        let conn = self.0.lock().unwrap();
        Self::verify(&conn, agent_id, user)?;
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

    // peek without consuming: how many inbox-class messages await this agent
    fn unread_count(&self, agent_id: i64) -> Result<i64, AgoraErr> {
        let conn = self.0.lock().unwrap();
        let (room, name, cursor): (String, String, i64) = conn.query_row(
            "SELECT room, name, cursor FROM agents WHERE id = ?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let n = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM messages
                 WHERE room = ?1 AND id > ?2 AND sender != ?3 AND kind NOT IN ({FEED_KINDS})
                   AND (recipient IS NULL OR recipient = ?3)"
            ),
            (&room, cursor, &name),
            |r| r.get(0),
        )?;
        Ok(n)
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

    fn set_status(&self, agent_id: i64, user: &str, status: &str) -> Result<(), AgoraErr> {
        let conn = self.0.lock().unwrap();
        Self::verify(&conn, agent_id, user)?;
        conn.execute("UPDATE agents SET status = ?1 WHERE id = ?2", (status, agent_id))?;
        Self::touch(&conn, agent_id);
        Ok(())
    }
}

fn db_err(e: rusqlite::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn agora_err(e: AgoraErr) -> McpError {
    match e {
        AgoraErr::Db(e) => db_err(e),
        AgoraErr::Denied => McpError::invalid_params(
            "denied: this agent name/id belongs to another user".to_string(),
            None,
        ),
    }
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
    fn join_room(
        &self,
        Parameters(p): Parameters<JoinParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let (id, backlog) = self
            .db
            .join(&p.room, &p.name, &p.harness, &p.machine, &caller(&parts))
            .map_err(agora_err)?;
        Ok(ok_json(json!({ "agent_id": id, "backlog": backlog })))
    }

    #[tool(description = "Post to your room. Broadcast by default; set `to` for a targeted message. Inbox kinds (msg/task/handoff/question/blocker) deliver to recipients; feed kinds (feed/summary/status/finding/decision/file_changed/test_result/review_finding) are ambient. source_id makes the post idempotent.")]
    fn post(
        &self,
        Parameters(p): Parameters<PostParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().unwrap_or("msg");
        let (id, new) = self
            .db
            .post(p.agent_id, &caller(&parts), &p.text, p.to.as_deref(), kind, p.source_id.as_deref())
            .map_err(agora_err)?;
        Ok(ok_json(json!({ "message_id": id, "new": new })))
    }

    #[tool(description = "Read recent ambient activity (kind=feed) from a room, optionally filtered to one agent. Never consumes inbox state; call only when you want to catch up on what peers are doing.")]
    fn feed(&self, Parameters(p): Parameters<FeedParams>) -> Result<CallToolResult, McpError> {
        let entries = self.db.feed(&p.room, p.from.as_deref(), p.limit.unwrap_or(20)).map_err(db_err)?;
        Ok(ok_json(json!({ "feed": entries })))
    }

    #[tool(description = "Block until a message arrives for you (or timeout), then return it like inbox. Terminal-agnostic wake: call this when idle and waiting for another agent.")]
    async fn wait_for_messages(
        &self,
        Parameters(p): Parameters<WaitParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&parts);
        let timeout = p.timeout_secs.unwrap_or(60).min(300);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
        loop {
            let msgs = self.db.inbox(p.agent_id, &user).map_err(agora_err)?;
            if !msgs.is_empty() || std::time::Instant::now() >= deadline {
                return Ok(ok_json(json!({ "messages": msgs })));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    #[tool(description = "Fetch messages you have not seen yet (each message is delivered exactly once). Call at the start of every turn.")]
    fn inbox(
        &self,
        Parameters(p): Parameters<AgentParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let msgs = self.db.inbox(p.agent_id, &caller(&parts)).map_err(agora_err)?;
        Ok(ok_json(json!({ "messages": msgs })))
    }

    #[tool(description = "List agents in a room with harness, machine, status, and idle time.")]
    fn peers(&self, Parameters(p): Parameters<PeersParams>) -> Result<CallToolResult, McpError> {
        let peers = self.db.peers(&p.room).map_err(db_err)?;
        Ok(ok_json(json!({ "peers": peers })))
    }

    #[tool(description = "Set your one-line status, visible to peers.")]
    fn set_status(
        &self,
        Parameters(p): Parameters<StatusParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.db.set_status(p.agent_id, &caller(&parts), &p.status).map_err(agora_err)?;
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

#[derive(serde::Deserialize)]
struct IngestReq {
    room: String,
    name: String,
    #[serde(default)]
    harness: String,
    #[serde(default)]
    machine: String,
    body: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
}

// non-consuming unread count, for turn-boundary hooks ("should I check inbox
// before going idle?"). Same shared secret as /ingest.
async fn unread(
    axum::extract::State(db): axum::extract::State<Arc<Db>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let expected = std::env::var("AGORA_INGEST_TOKEN").unwrap_or_default();
    let given = headers.get("x-agora-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if expected.is_empty() || given != expected {
        return Err((StatusCode::FORBIDDEN, "bad or missing x-agora-token".into()));
    }
    let agent_id: i64 = q
        .get("agent_id")
        .and_then(|s| s.parse().ok())
        .ok_or((StatusCode::BAD_REQUEST, "agent_id required".into()))?;
    let n = db
        .unread_count(agent_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}")))?;
    Ok(axum::Json(json!({ "unread": n })))
}

// plain HTTP door for the scribe daemon, gated by a shared secret
async fn ingest(
    axum::extract::State(db): axum::extract::State<Arc<Db>>,
    headers: axum::http::HeaderMap,
    axum::Json(req): axum::Json<IngestReq>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let expected = std::env::var("AGORA_INGEST_TOKEN").unwrap_or_default();
    let given = headers.get("x-agora-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if expected.is_empty() || given != expected {
        return Err((StatusCode::FORBIDDEN, "bad or missing x-agora-token".into()));
    }
    let err = |e: AgoraErr| match e {
        AgoraErr::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        AgoraErr::Denied => (StatusCode::FORBIDDEN, "name belongs to another user".into()),
    };
    let (agent_id, _) = db.join(&req.room, &req.name, &req.harness, &req.machine, "owner").map_err(err)?;
    let (id, new) = db
        .post(agent_id, "owner", &req.body, None, req.kind.as_deref().unwrap_or("summary"), req.source_id.as_deref())
        .map_err(err)?;
    Ok(axum::Json(json!({ "message_id": id, "new": new })))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("AGORA_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let db_path = std::env::var("AGORA_DB").unwrap_or_else(|_| "agora.db".into());
    let db = Arc::new(Db::open(&db_path)?);
    let db_state = db.clone();

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
    let router = axum::Router::new()
        .route("/ingest", axum::routing::post(ingest))
        .route("/unread", axum::routing::get(unread))
        .with_state(db_state)
        .nest_service("/mcp", service);
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
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();

        db.post(a, "owner", "hi bob", None, "msg", None).unwrap();
        let got = db.inbox(b, "owner").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["body"], "hi bob");
        // second read: nothing new, no duplicates (the Mosaic flaw)
        assert!(db.inbox(b, "owner").unwrap().is_empty());
        // sender never sees own message
        assert!(db.inbox(a, "owner").unwrap().is_empty());
    }

    #[test]
    fn targeted_messages_skip_others() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();
        let (c, _) = db.join("r", "carol", "", "", "owner").unwrap();

        db.post(a, "owner", "for bob only", Some("bob"), "msg", None).unwrap();
        assert_eq!(db.inbox(b, "owner").unwrap().len(), 1);
        assert!(db.inbox(c, "owner").unwrap().is_empty());
        // carol's cursor advanced past the filtered message; nothing re-delivers later
        assert!(db.inbox(c, "owner").unwrap().is_empty());
    }

    #[test]
    fn rejoin_keeps_cursor_and_backlog_shows_unseen() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();
        db.post(a, "owner", "one", None, "msg", None).unwrap();
        assert_eq!(db.inbox(b, "owner").unwrap().len(), 1);
        db.post(a, "owner", "two", None, "msg", None).unwrap();

        // bob rejoins (new session): same id preserved, cursor intact,
        // inbox still delivers only the unseen "two"
        let (b2, backlog) = db.join("r", "bob", "codex", "mac", "owner").unwrap();
        assert_eq!(b, b2);
        assert!(!backlog.is_empty());
        let got = db.inbox(b, "owner").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["body"], "two");
    }

    #[test]
    fn feed_is_ambient_not_inbox() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();

        db.post(a, "owner", "turn 1: refactoring auth", None, "feed", None).unwrap();
        db.post(a, "owner", "hey bob, need review", None, "msg", None).unwrap();

        // inbox delivers only the msg, never feed entries
        let got = db.inbox(b, "owner").unwrap();
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
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();

        let (m1, n1) = db.post(a, "owner", "turn text", None, "summary", Some("uuid-1")).unwrap();
        let (m2, n2) = db.post(a, "owner", "turn text", None, "summary", Some("uuid-1")).unwrap();
        assert_eq!(m1, m2); assert!(n1); assert!(!n2); // scribe can re-scan transcripts safely
        assert_eq!(db.feed("r", None, 20).unwrap().len(), 1);
        let _ = b;
    }

    #[test]
    fn kind_classes_route_correctly() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "", "owner").unwrap();
        let (b, _) = db.join("r", "bob", "", "", "owner").unwrap();

        db.post(a, "owner", "urgent", Some("bob"), "blocker", None).unwrap();
        db.post(a, "owner", "tests green", None, "test_result", None).unwrap();

        // blocker -> inbox; test_result -> feed only
        let got = db.inbox(b, "owner").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["kind"], "blocker");
        let f = db.feed("r", None, 20).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["kind"], "test_result");
    }

    #[test]
    fn other_user_cannot_spoof_or_read() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "", "", "gianni").unwrap();
        db.post(a, "gianni", "private note", None, "msg", None).unwrap();

        // friend can't act as alice's agent_id or claim her name
        assert!(matches!(db.inbox(a, "friend"), Err(AgoraErr::Denied)));
        assert!(matches!(db.post(a, "friend", "spoof", None, "msg", None), Err(AgoraErr::Denied)));
        assert!(matches!(db.set_status(a, "friend", "x"), Err(AgoraErr::Denied)));
        assert!(matches!(db.join("r", "alice", "", "", "friend"), Err(AgoraErr::Denied)));

        // friend can join under their own name; owner rejoin still fine
        assert!(db.join("r", "bob", "", "", "friend").is_ok());
        assert!(db.join("r", "alice", "", "", "gianni").is_ok());
    }

    #[test]
    fn peers_and_status() {
        let db = mem();
        let (a, _) = db.join("r", "alice", "claude-code", "mac", "owner").unwrap();
        db.set_status(a, "owner", "building agora").unwrap();
        let peers = db.peers("r").unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["status"], "building agora");
        assert_eq!(peers[0]["harness"], "claude-code");
    }
}

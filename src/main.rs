use std::sync::{Arc, Mutex};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use rusqlite::Connection;
use serde_json::json;

// ponytail: one Mutex<Connection>, all DB ops sync. Per-room locks if this ever has real load.
struct Db(Mutex<Connection>);

const BACKLOG: i64 = 20;
const WINDOW: i64 = 5000; // rolling message window kept in the db

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
                created INTEGER DEFAULT (unixepoch())
            );",
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

    fn post(&self, agent_id: i64, text: &str, to: Option<&str>) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        let (room, name): (String, String) = conn.query_row(
            "SELECT room, name FROM agents WHERE id = ?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        conn.execute(
            "INSERT INTO messages(room, sender, recipient, body) VALUES(?1, ?2, ?3, ?4)",
            (&room, &name, to, text),
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
        let mut stmt = conn.prepare(
            "SELECT id, sender, recipient, body, created FROM messages
             WHERE room = ?1 AND id > ?2 AND sender != ?3
               AND (recipient IS NULL OR recipient = ?3)
             ORDER BY id DESC LIMIT ?4",
        )?;
        let mut rows: Vec<serde_json::Value> = stmt
            .query_map((room, after, me, limit.unwrap_or(i64::MAX)), |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "from": r.get::<_, String>(1)?,
                    "to": r.get::<_, Option<String>>(2)?,
                    "body": r.get::<_, String>(3)?,
                    "at": r.get::<_, i64>(4)?,
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

    #[tool(description = "Post a message to your room. Broadcast by default; set `to` for a targeted message.")]
    fn post(&self, Parameters(p): Parameters<PostParams>) -> Result<CallToolResult, McpError> {
        let id = self.db.post(p.agent_id, &p.text, p.to.as_deref()).map_err(db_err)?;
        Ok(ok_json(json!({ "message_id": id })))
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

    let service = StreamableHttpService::new(
        move || Ok(Agora::new(db.clone())),
        LocalSessionManager::default().into(),
        Default::default(),
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

        db.post(a, "hi bob", None).unwrap();
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

        db.post(a, "for bob only", Some("bob")).unwrap();
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
        db.post(a, "one", None).unwrap();
        assert_eq!(db.inbox(b).unwrap().len(), 1);
        db.post(a, "two", None).unwrap();

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

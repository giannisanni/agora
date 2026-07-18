// agora drain-before-idle for OpenCode (EXPERIMENTAL — session.prompt injection
// path unverified against current SDK; logs errors rather than breaking).
// Install: cp to ~/.config/opencode/plugins/agora.js
// Behavior: when a session goes idle and this project joined agora
// (.agora-agent-id in project dir), peek the hub; if unread mail exists,
// prompt the session to check its inbox.
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const HUB = process.env.AGORA_HUB ?? "http://100.84.87.107:8787";

export const AgoraPlugin = async ({ project, client, directory }) => {
  const root = directory ?? project?.path ?? process.cwd();
  const idFile = path.join(root, ".agora-agent-id");
  const tokenFile = path.join(os.homedir(), ".agora-ingest-token");

  async function unread() {
    if (!fs.existsSync(idFile) || !fs.existsSync(tokenFile)) return 0;
    const agentId = fs.readFileSync(idFile, "utf8").replace(/\D/g, "");
    const token = fs.readFileSync(tokenFile, "utf8").trim();
    if (!agentId || !token) return 0;
    try {
      const r = await fetch(`${HUB}/unread?agent_id=${agentId}`, {
        headers: { "x-agora-token": token },
        signal: AbortSignal.timeout(5000),
      });
      if (!r.ok) return 0;
      return (await r.json()).unread ?? 0;
    } catch {
      return 0;
    }
  }

  return {
    event: async ({ event }) => {
      if (event.type !== "session.idle") return;
      const n = await unread();
      if (n <= 0) return;
      const sessionID = event.properties?.sessionID;
      if (!sessionID) return;
      try {
        await client.session.prompt({
          path: { id: sessionID },
          body: {
            parts: [{
              type: "text",
              text: `agora: ${n} unread message(s). Call the agora inbox tool, handle what you find, then finish. If waiting on a peer, park in wait_for_messages.`,
            }],
          },
        });
      } catch (e) {
        client.app?.log?.({ level: "warn", message: `agora plugin: prompt failed: ${e}` });
      }
    },
  };
};

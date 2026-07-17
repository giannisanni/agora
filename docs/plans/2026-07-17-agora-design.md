# agora — agent linking hub

Design doc, 2026-07-17. Status: approved for phase 1.

## Problem

Mosaic's agent-room links agent sessions, but badly:

- Passive relay re-injects the same messages every turn (no read cursor) — floods context.
- Linking rides on per-harness hooks. Claude Code works; Codex stayed `hook:MISSING`
  even after `mosaic hooks setup codex` installed `~/.codex/hooks.json`. Live-tested 2026-07-17.
- The relay needs a pre-turn injection point (Claude Code's `UserPromptSubmit`).
  Most harnesses don't have one.
- Binding uses tty/pid/env heuristics that flake (`hook:linked` toggled to
  `hook:MISSING` between queries on a healthy session).
- Single-machine, single-app (Mosaic) lock-in.

## Goal

Link AI coding agents across harnesses (Claude Code, Codex, OpenCode, Gemini,
Cursor, …), machines (Tailscale mesh + friends over the internet), OSes, and
terminals. Zero per-harness integration code in phase 1.

## Core insight

MCP-over-HTTP is the only interface every harness already speaks. So the whole
linking layer is one MCP server. No hooks, no terminal automation, no tty/pid
binding.

## Architecture

One Rust binary on substrate. Stack: rmcp (official Rust MCP SDK, streamable
HTTP transport) + axum + rusqlite.

- Personal machines: reach it over Tailscale (`http://substrate:<port>/mcp`).
- Friends: same server exposed via cloudflared + auth (existing
  `mcp-x.giannisan.com` pattern). Bearer token per user.
- Storage: single SQLite file. Messages in a rolling window (prune old).

Hub, not P2P: at "me + a few friends" scale, P2P buys nothing and costs
discovery, NAT traversal, ordering, and offline delivery. Tailscale already
provides the mesh transport for personal machines.

## API — 5 MCP tools

| Tool | Args | Returns |
|---|---|---|
| `join_room` | room, name (machine/harness optional metadata) | agent_id + recent backlog |
| `post` | room, text, to? (agent name for targeted) | ack |
| `inbox` | — | only messages this agent hasn't seen (per-agent cursor) |
| `peers` | room | members: name, harness, machine, status, last_seen |
| `set_status` | text | ack |

Presence = heartbeat from last tool call. No process inspection.

## Delivery model

Pull-on-turn. Each harness gets one line in its instructions file
(`CLAUDE.md`, `AGENTS.md`, `.cursorrules`, …): "call agora `inbox` at turn
start; when handing work off, call `post`." Instructions files are the one
integration point every harness has — prompt-level, not hook-level.

Known trade-off: an idle agent doesn't see messages until its next turn.
Accepted for phase 1; the wake shim (below) buys it back later.

## Fixes mapped to Mosaic flaws

| Mosaic flaw | agora answer |
|---|---|
| Duplicate relay every turn | per-agent read cursor; `inbox` returns each message once |
| Per-harness hook installs | none — MCP tools only |
| Needs pre-turn injection hook | instructions-file line instead |
| tty/pid binding flakiness | heartbeat presence |
| App/OS lock-in | plain MCP + HTTP + SQLite |

## Deferred (YAGNI until proven needed)

- Wake shim: optional per-machine daemon; polls hub for undelivered mail to
  idle local agents, types into their tmux pane. Phase 2, opt-in.
- File/artifact transfer, nested rooms, long-term message persistence,
  message threading, delivery receipts.

## Prior art

- [claude-peers-mcp](https://github.com/louislva/claude-peers-mcp) — broker +
  register + poll skeleton borrowed as the pattern. Same-machine, CC-only push;
  we generalize both.
- [Agentrooms / claude-code-by-agents](https://github.com/baryhuang/claude-code-by-agents)
  — validated the room/@mention UX; rejected as base (Swift app + OpenAgents
  protocol lock-in).
- A2A (Google/Linux Foundation) — message-format ideas only; too heavy.

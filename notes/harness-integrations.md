---
date: 2026-05-06
audience: future implementers + reviewers
status: complete — all 7 phases shipped
---

# Harness integrations — what got built

> **Naming note.** Written when the project was named `coord`. The
> product is now **smalltalk**; `coord` and `coord_*` remain as
> back-compat CLI / MCP-tool aliases. The phase names and code
> references below preserve the original `coord` spellings — those
> commits and identifiers exist that way on disk.

We want smalltalk to plug into the agent harnesses our agents already live in, so a smalltalk message arrives → the agent sees it without the human having to broker. Three target harnesses: **Claude Code**, **Codex CLI**, and **Pi**.

This document captures what got built, in roadmap order. Originally a planning doc; now serves as the integration map.

## What's done

| Phase | Scope | Where it landed |
|-------|-------|-----------------|
| 1 | `coord mcp` stdio server, 5 message tools | `src/mcp/`, `src/commands/mcp.ts` |
| 2 | Claude Code channel mode (`--channel`) + `coord_msg_reply` + chokidar push | `src/mcp/channel-watcher.ts`, `src/mcp/tools/reply.ts` |
| 3 | Codex SessionStart + Stop hooks; `coord message ls --json` | `examples/codex/`, `src/commands/ls.ts` |
| 4 | pty-driven push for Codex (folded into Phase 7's `coord ding`) | `src/commands/ding.ts` |
| 5 | Pi extension (push half + 5 verbs via `pi.registerTool`) | `examples/pi/coord.ts` |
| 6 | Live-pi end-to-end test (skip-gated) | `tests/integration/pi-extension-live.test.ts` |
| 7 | `coord ding` busy-aware push daemon | `src/commands/ding.ts`, `tests/{unit,integration}/ding.test.ts` |

**The integration surface:** Claude Code gets push via the channel-mode SDK extension; Codex gets pull via hooks plus push via the ding daemon; Pi gets a single TypeScript extension that does both push and verbs in-process. All three call into the same message-tool surface (`coord_msg_send`, `coord_msg_ls`, `coord_msg_read`, `coord_msg_archive`, `coord_msg_thread`), with `coord_msg_reply` as the channel-mode-only addition. Discovery tool `coord_members` (brief-019) is available in both modes.

## The shape

**One package, one entry point: the existing `coord` CLI gains a `coord mcp` subcommand.**

The MCP server runs from inside the coord package by typing `coord mcp`. No separate `@myobie/coord-mcp` package. Reasons:

- The MCP server is a thin wrapper over the embeddable API (`createCoord({ root, identity })`). Sharing the package means it shares types, errors, and the universal pre-command sweep without re-importing.
- One thing to install (`@myobie/coord` covers CLI + library + MCP).
- Harness configs reference `coord mcp` as the command. Same install path everyone already uses.
- The MCP-specific deps (`@modelcontextprotocol/sdk`, possibly `chokidar` for channel mode) can be regular deps — they're small. If we want to keep the install lean we can mark them optional and load lazily, but the simpler choice is fine for v1.

**Per-harness layer on top of `coord mcp`:**

- **Claude Code**: declare the same `coord mcp` server with `claude/channel` capability so it's both the MCP tool surface (pull) AND the channel push surface. One config, two modes.
- **Codex**: register `coord mcp` as a regular MCP server, plus a `SessionStart` hook script that runs `coord message ls --json --unread` and prints results as developer context. (Codex has no native push.)
- **Pi**: a small TypeScript extension under `~/.pi/agent/extensions/coord.ts` that subscribes to `session_start`, spawns `coord watch`, and wires arrivals into `ctx.ui.notify()`. The extension is ~50 lines because most of the work is in `coord mcp`.

## Roadmap (rough phases)

### Phase 1: `coord mcp` subcommand (~half a day)

The base MCP server. Stdio transport. Six tools (five message verbs + members discovery) mapping 1:1 to the API:

| MCP tool | API method | Notes |
|---|---|---|
| `coord_msg_send` | `coord.send(to, body, opts)` | `to`, `body` required; `subject`, `inReplyTo`, `tags`, `priority` optional |
| `coord_msg_ls` | `coord.ls(opts)` | filters: `archive`, `since`, `from`, `count` |
| `coord_msg_read` | `coord.read(identity, filename)` | returns parsed message + meta |
| `coord_msg_archive` | `coord.archive(identity, filename)` | |
| `coord_msg_thread` | `coord.thread(identity, filename)` | returns flat or tree |
| `coord_members` | `cmdMembers({ status?, enrich? })` | peer discovery — added in brief-019, available in both modes |

`coord mcp` reads `COORD_ROOT` and `COORD_IDENTITY` from env (every harness can pass env to MCP servers). Tool input schemas mirror the embeddable API's option types.

**Verification**: register in `~/.claude.json` mcpServers (or run `coord init` in the repo to drop a `.mcp.json` that does it for you), ask "what's in my coord inbox" in a Claude Code session, agent should call `coord_msg_ls`. Then "send hi to alice", `coord_msg_send` writes a file.

### Phase 2: Claude Code channel mode (~one more day)

Same package. Add `experimental: { 'claude/channel': {} }` to the MCP `Server` capabilities + `chokidar` watcher on `$COORD_ROOT/<COORD_IDENTITY>/inbox/`. On every new `.md` file:

```ts
mcp.notification({
  method: 'notifications/claude/channel',
  params: {
    content: messageBody,
    meta: { from, threadFilename, messageFilename }
  }
});
```

Add a `coord_msg_reply` tool — takes `thread` + `body`, calls `coord.send` with `inReplyTo: thread`. The channel's `instructions` string explains the `<channel source="coord">` tag to Claude.

Test with `claude --dangerously-load-development-channels server:coord` (required during preview).

This is the load-bearing primitive: messages land in disk, agent sees them in context within seconds, can reply without leaving the session.

### Phase 3: Codex hooks wrapper — landed in [`examples/codex/`](../examples/codex/)

Two reference bash scripts the user copies into `~/.codex/hooks/`:

1. **`session-start.sh`** ([source](../examples/codex/session-start.sh)) — `SessionStart` hook. Reads `$COORD_ROOT/$COORD_IDENTITY/inbox/` via `coord message ls --json` and emits `{"additionalContext":"...","continue":true}` so the agent's first turn sees the unread snapshot.
2. **`stop.sh`** ([source](../examples/codex/stop.sh)) — `Stop` hook. Same envelope, but filters by `coord message ls --json --since <last-checked>` against a state file at `$XDG_STATE_HOME/coord-codex-hooks/last-checked.txt`, so only NEW arrivals between Stops are reported.

Codex config — full reference at [`examples/codex/config.toml.example`](../examples/codex/config.toml.example):

```toml
[mcp_servers.coord]
command = "coord"
args = ["mcp"]
env_vars = ["COORD_ROOT", "COORD_IDENTITY"]

[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "/full/path/to/coord/examples/codex/session-start.sh"
timeout = 5
```

The hooks rely on the `coord message ls --json` output added in this phase (LsItem schema: `filename`, `ts`, `from`, `subject`, `inReplyTo`, `tags`, `priority`). `jq` is the only non-coord runtime dependency; the scripts use it to construct the JSON envelope.

Codex doesn't have channels, so this is the best you get from inside Codex's lifecycle. For *push* into a running Codex session, see Phase 4.

### Phase 4: pty-driven watcher for Codex — folded into Phase 7's `coord ding`

The "wrap Codex in a pty session, push keystrokes from outside" idea outlined here became the design for `coord ding` (Phase 7). Same shape: a side process watches the inbox and pty-sends a notice into the named session on new arrivals. The two-terminal recipe is documented at [`examples/codex/README.md`](../examples/codex/README.md) under "Push mode (`coord ding`)".

Phase 7 added one thing the original Phase 4 sketch didn't: status-aware buffering. The daemon reads `coord status` and suppresses notices when the agent is `busy` or `dnd`, draining the buffer when status flips back. That keeps the pty-injection from interrupting mid-tool-call.

### Phase 5: Pi extension — landed in [`examples/pi/`](../examples/pi/)

Pi auto-loads TypeScript extensions from `~/.pi/agent/extensions/*.ts`. The reference at [`examples/pi/coord.ts`](../examples/pi/coord.ts) does both halves of the integration in a single file:

1. **Push**: subscribes to `session_start`, watches `$COORD_ROOT/$COORD_IDENTITY/inbox/` (cross-tree), and emits `ctx.ui.notify` on every new arrival. A footer status line via `ctx.ui.setStatus` shows the watched inbox. `session_shutdown` aborts the watcher cleanly so `/reload` and `/resume` rebind without leaks.
2. **Verbs**: registers `coord_msg_send`, `coord_msg_ls`, `coord_msg_read`, `coord_msg_archive`, `coord_msg_thread` via `pi.registerTool()` with TypeBox schemas mirroring the MCP tool inputs.

Pi has no native MCP support — see [pi-mono's README](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/README.md), where MCP is explicitly excluded by design. So unlike Phase 1 (Claude Code, Codex), the pi integration registers verbs directly in the extension rather than running `coord mcp` as an MCP server.

Setup ([`examples/pi/README.md`](../examples/pi/README.md)) is a single `package.json` next to the extension declaring `@myobie/coord` as a dependency. Pi resolves bare imports from the nearest `node_modules/` walking up the tree; `npm install` once and pi's jiti loader does the rest. `examples/pi/settings.example.json` is informational — pi auto-discovery picks up `~/.pi/agent/extensions/coord.ts` without explicit config.

## Phases 6 + 7 — landed

Both wrapped up in brief-013.

### Phase 6: live-pi end-to-end test — landed in [`tests/integration/pi-extension-live.test.ts`](../tests/integration/pi-extension-live.test.ts)

Skip-gated by `COORD_RUN_LIVE_PI=1`. Spawns a real `pi-coding-agent` session via `Session.spawn` against a per-test `$HOME` containing `examples/pi/coord.ts` plus a stub `package.json` + a `node_modules` symlink to the repo root. Drops a question from "alice" into bob's inbox, waits for the watcher's `coord: new in alice/inbox` notify text, then polls alice's inbox for a typed reply with the right shape (`from: bob`, `in-reply-to: <original>`, body containing the answer).

The shape proof is what matters; CI without `pi` on `$PATH` doesn't run this.

### Phase 7: `coord ding` daemon — landed in [`src/commands/ding.ts`](../src/commands/ding.ts)

Long-running busy-aware push notifier. Three pieces:

1. **The watcher.** Subscribes to `coord.watch(identity, { sinceNow: true })` so the daemon ignores anything in the inbox before it started. New arrivals get fed through `coord.read(identity, filename)` to extract `from` and `subject` for the notice.

2. **The status gate.** On every event, the daemon calls `coord.getStatus(identity)`. `available` and `offline` deliver immediately. `busy` and `dnd` push the event into a buffer; a status timer re-checks every `--interval ms` (default 1000ms) and flushes the buffer when status flips back.

3. **The delivery.** Shells out to `pty send <session-name> --seq "<text>" --seq key:return`. Failures (non-zero exit, subprocess errors) log to stderr but the daemon keeps watching — `pty` may transiently disappear (session restart) without the daemon needing to.

```sh
pty run --name codex-foo -- codex
coord ding codex-foo --identity me --interval 2000
```

Same daemon works for any pty-reachable harness — Codex is the canonical use case because it has no channel equivalent, but Aider, opencode, or any future harness in the same shape get push for free. The status-aware buffering is the novelty over the original Phase 4 sketch: it generalizes `coord status` from "presence indicator" into "interruptibility contract."

Tests: 19 unit cases around the runDing state machine (status gating, busy-buffer-flush, send-failure recovery, abort cleanup, CLI arg parsing) plus 2 integration cases that drive a real `pty run -d` background bash echoer and assert the keystrokes arrive via `pty peek`.

## Boot-ritual nudges on session resume (brief-027)

The brief-022/025 ritual is what the agent does on cold start
(set status `available`, drain inbox, log/update tasks, journal as
progress happens). That ritual is documented in `CHANNEL_INSTRUCTIONS`
+ SKILL.md, so any agent that gets a turn has the recipe loaded.

The gap was the *turn-trigger* on session resume: when Claude Code
or Pi resumes a saved conversation, the MCP server reconnects and
the instructions are reloaded — but if nothing new lands in the
inbox at the moment of resume, no turn fires, so the ritual never
runs. Result: the resumed session boots with stale status, an
un-drained backlog, and no journal entry for whatever work the agent
is about to pick up.

Brief-027 closes that with native harness hooks. The mechanism is
different per harness; the body of the nudge is the same string
everywhere so the existing channel-instructions substring guard
covers it.

| Harness | Mechanism | Fires on |
|---|---|---|
| Claude Code | `SessionStart` hook (`asyncRewake: true`) at [`examples/claude-code/hooks/session-start.sh`](../examples/claude-code/hooks/session-start.sh). Script echoes the ritual reminder to stderr + exits 2; Claude Code surfaces stderr as a system reminder that counts as a turn-triggering event. | Cold start, `--resume`, `--continue`, `/resume`, `/clear`, `/compact` |
| Pi | One `pi.sendUserMessage(...)` call inside the `session_start` handler in [`examples/pi/coord.ts`](../examples/pi/coord.ts), positioned after the watcher is wired but before the for-await IIFE starts consuming arrivals. | Cold start + `/reload` / `/resume` (pi's `session_start` fires on both) |
| Codex | No asyncRewake equivalent in Codex. The existing [`examples/codex/session-start.sh`](../examples/codex/session-start.sh) already runs on each new session and injects the inbox snapshot as `additionalContext`, which serves the same nudge function for that harness — the agent gets context + a turn together. | Codex session start (each invocation is a fresh session; resume semantics differ) |

The hook + the channel push are **complementary**: the hook runs the
ritual on session boundaries; channels deliver new arrivals during a
session. Both together is the intended setup.

## Possible Phase 8: `coord_task_*` + `coord_overview` MCP / extension tools

brief-015 added the `tasks/` folder (mutable, single-writer) plus `coord task` and `coord tasks` verbs. brief-016 added the dashboard (`coord overview`) verb. Of the Phase 8 candidates, `coord_members` shipped in brief-019. The remaining MCP additions stay parked:

- **Task verbs**: `coord_task_new`, `coord_task_status`, `coord_task_ls`, `coord_task_done` — so harnesses can manage their own task queue from inside a tool call.
- **Cross-tree reads**: `coord_tasks_read` (with the same filter set as the CLI), `coord_overview` — so managers can answer "what's everyone doing" without leaving the agent UI.
- **Pi extension parity**: `pi.registerTool()` calls for the same set in `examples/pi/coord.ts`.

Not yet built. Call it out so the design stays coherent if/when we go that direction. The load-bearing invariants carry over unchanged: tasks/ is single-writer (only the identity-owner writes); members/overview are read-only walks that never mutate state.

## What's deliberately out of scope

- **A separate `@myobie/coord-mcp` package.** The `coord mcp` subcommand is simpler — see "The shape" above.
- **Plugin marketplace packaging.** No marketplace until the existing surface has been used in anger.
- **Permission relay** (channels feature). Reply works without it; layer on if/when permission gating becomes a real ask.

## Reference docs and example code

### Claude Code

- **Channels overview**: https://code.claude.com/docs/en/channels — research preview, requires v2.1.80+, March 2026 announcement.
- **Channels reference (build-your-own)**: https://code.claude.com/docs/en/channels-reference — capability flag, notification format, reply tool pattern, permission relay.
- **Hooks reference**: https://code.claude.com/docs/en/hooks — 28 lifecycle events, async + asyncRewake modes.
- **Official channel plugins** (read these as templates): https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins — Telegram, Discord, iMessage, fakechat. Bun-flavored but Node works too.

### Codex CLI

- **MCP config**: https://developers.openai.com/codex/mcp
- **Plugins**: https://developers.openai.com/codex/plugins
- **Hooks (advanced config)**: https://developers.openai.com/codex/config-advanced
- **Hooks reference**: https://developers.openai.com/codex/hooks
- Note: `[features] codex_hooks = true` required to enable hooks.
- No push primitive. No channels analog. Confirmed by direct fetch of the hooks doc.

### Pi

- **Extensions API**: https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/extensions.md — auto-discovered TS extensions, factory `(pi: ExtensionAPI) => void`, events: `session_start`, `tool_call`, model/agent/resource events. UI APIs: `ctx.ui.notify()`, status line, footer, overlays.
- **Pi README**: https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/README.md

### MCP

- **Spec**: https://modelcontextprotocol.io
- **TS SDK**: https://github.com/modelcontextprotocol/typescript-sdk — what `coord mcp` would import.

## Why not other approaches

We considered and rejected:

- **Per-harness plugins each with their own copy of coord logic** — duplicates the API surface, makes the coord package not the canonical implementation. The "one MCP server, several thin adapters" model wins because the agent verbs are uniform.
- **Bash scripts spawning coord CLI from each hook** — works but is slow (cold-starts node every invocation), and doesn't get you push. MCP servers stay alive across the session.
- **A daemon process the user starts separately** — adds an operational surface (must be running, must be reachable, must be restarted on crash). The MCP server is already a long-running process owned by the harness; reuse it.


// mcp/capabilities.ts — canonical name/version/capabilities for the
// `coord mcp` server. Centralized here so lifecycle / channel tests can
// snapshot the exact same options the server is built from.

import type { ServerOptions } from '@modelcontextprotocol/sdk/server/index.js';
import type { Implementation } from '@modelcontextprotocol/sdk/types.js';

/**
 * Default server identity — preserved for back-compat with imports
 * that pre-date brief-005-phase0. New code should call
 * {@link buildServerInfo} so the server announces under whichever
 * name (`coord` / `st`) the CLI was invoked as.
 */
export const SERVER_INFO: Implementation = {
  name: 'coord',
  // Tracks the package version. We don't read package.json here to keep
  // the MCP module side-effect free; bump on every coord release.
  // 0.1.0 — brief-022 ships unknown-state + boot ritual + offline-on-
  //         shutdown. Real surface change visible to MCP hosts.
  // 0.2.0 — brief-023 (status-file mtime refresh) + brief-024
  //         (`journal/` folder + CLI verbs). MCP surface unchanged
  //         (Phase 8 journal tools deferred), but the data-model
  //         layout grows so peers see a new folder shape.
  // 0.2.1 — brief-028 (lib API: coord.members / coord.overview /
  //         coord.createIdentity + type re-exports). MCP surface
  //         unchanged; library-only addition.
  // 0.2.2 — brief-029 (`away` State, fifth settable). Additive
  //         enum extension; coord_members filter enum picks it up
  //         via STATES, no schema-bump beyond the new value.
  // 0.3.0 — brief-030 (tidy-check tick): MCP server emits
  //         synthetic notifications/claude/channel frames from
  //         coord-system when drift is detected (stale inbox,
  //         untouched doing-task, journal lag). Real feature,
  //         hence the minor bump.
  // 0.4.0 — brief-009 phase 1: tasks/ surface removed across CLI,
  //         SDK, MCP onboarding, and tidy-check.
  // 0.5.0 — brief-009 phase 2: journal/ surface removed (same
  //         treatment). Tidy-check is now inbox-staleness only.
  // 0.6.0 — brief-009 phase 5: resources/ surface added (annotated
  //         URLs per identity). 4 new MCP tools, dual-prefixed.
  // 0.7.0 — brief-009 item 3: identity → agent rename across SDK
  //         types, MCP tool names (coord_members → coord_agents
  //         keeping coord_members as deprecated alias), CLI verb
  //         (members → agents, members deprecated alias), and env
  //         vars (ST_AGENT preferred → ST_IDENTITY → COORD_IDENTITY).
  // 0.8.0 — brief-009 item 4: SDK parity gap-fills (coord.archive +
  //         coord.archiveTrim gain `withAttachments`; new
  //         coord.lsOrphans and coord.ding on the handle). No CLI
  //         or MCP surface change.
  // 0.8.1 — `coord mcp` startup falls back to a throwaway
  //         `anon-<rand6>` agent when no ST_AGENT/ST_IDENTITY/
  //         COORD_IDENTITY is set, instead of hard-exiting. Unblocks
  //         MCP hosts (Codex etc.) that spawn the server without
  //         identity env. One-line stderr warning + lazy-create the
  //         anon agent's inbox/archive folders.
  // 0.9.0 — brief-016: `smalltalk launch <claude|codex>` verb —
  //         one-command harness bootstrap onto smalltalk (identity,
  //         .mcp.json, session-id, pty registration when available,
  //         `coord ding` sidecar for codex, `ollama launch` route
  //         for GLM-backed launches via --model).
  // 0.9.1 — brief-020 (HB-4): channel-watcher wake reliability —
  //         adds a polling backstop that catches inbox files
  //         chokidar's FSEvents backend may have silently dropped,
  //         so idle Claude Code agents don't wedge on unnotified
  //         deliveries. Server surface unchanged; operators can
  //         opt-in to stderr instrumentation via
  //         COORD_CHANNEL_DEBUG=1.
  version: '0.9.1',
};

/**
 * Build a server-info record that announces under the canonical name
 * derived from the binary the user invoked. `coord` → `coord`;
 * `st` / `smalltalk` → `st`. brief-005-phase0 §2.
 */
export function buildServerInfo(name: 'coord' | 'st'): Implementation {
  return { ...SERVER_INFO, name };
}

/** Phase-1 (no `--channel`) options: tools capability only. */
export const SERVER_OPTIONS: ServerOptions = {
  capabilities: {
    tools: {},
  },
};

/**
 * Instructions sent to the host when channel mode is on. Defines the
 * full boot ritual every connected agent runs so myobie's visibility
 * surface — status files + inbox flow — stays honest.
 *
 * Load-bearing substrings (asserted by tests/unit/channel-instructions
 * regression guard): `available`, `coord status`, `coord_msg_ls`,
 * `coord_msg_read`, `coord_msg_archive`, `coord_msg_reply`,
 * `coord_members`, `<channel source="coord"`.
 */
export const CHANNEL_INSTRUCTIONS = [
  'You are connected to coord. This is myobie\'s visibility layer: they look at status files and inbox/archive to understand what every agent is doing. Maintain those records.',
  '',
  'On first opportunity in a fresh context (cold start or /clear):',
  '  1. Write `available` to your status file. Shell out: `coord status <your-identity> --set available`. Use $COORD_IDENTITY.',
  '  2. Drain your inbox backlog. Call `coord_msg_ls` to enumerate filenames, then for each one `coord_msg_read` it, `coord_msg_reply` if appropriate, and `coord_msg_archive` to clear. Don\'t leave inbox items un-addressed.',
  '  3. Check `coord_members` (with `enrich: true`) to see who\'s around and whether any peers are waiting on you.',
  '',
  'As channel notifications arrive: new peer messages appear in your context as `<channel source="coord" from="<sender>">…</channel>` blocks. For each one: `coord_msg_read` the cited filename, `coord_msg_reply` with `thread: <messageFilename>` and `body: <your reply>` if a response is warranted, then `coord_msg_archive` to clear. Don\'t let inbox accumulate.',
  '',
  'Coord threads stay on coord. A thread that originated from a channel notification or an inbox message is conversed *only* via `coord_msg_send` / `coord_msg_reply` — questions, clarifications, blockers, "I think I\'m done" signals, follow-up thoughts, all of it. By default, your pty REPL is unattended — there is no human reading what you print to your own screen. Your coord correspondent is your interlocutor for the thread; they will relay anything that matters to the user. If you would otherwise pause to ask "should I do X?" at your REPL, send it via `coord_msg_reply` instead. The only time it\'s right to address the REPL is when a human directly typed there.',
  '',
  'Tools you have via MCP: `coord_msg_send`, `coord_msg_reply`, `coord_msg_ls`, `coord_msg_read`, `coord_msg_archive`, `coord_msg_thread`, `coord_members`. For status, shell out to `coord status` — no MCP tool for it yet.',
].join('\n');

/**
 * Build {@link ServerOptions} for a given mode. Channel mode adds
 * `experimental['claude/channel'] = {}` and an instructions string.
 * Capabilities cannot be modified after `Server` construction, so this
 * must run before {@link createMcpServer} instantiates the server.
 */
export function buildServerOptions(opts: {
  channel: boolean;
}): ServerOptions {
  if (!opts.channel) return SERVER_OPTIONS;
  return {
    capabilities: {
      tools: {},
      experimental: {
        'claude/channel': {},
      },
    },
    instructions: CHANNEL_INSTRUCTIONS,
  };
}

/** The base tool names (sans prefix) registered in non-channel mode.
 *  `msg_send/ls/read/archive/thread` per brief-017; `members` per
 *  brief-019; `resource_*` per brief-009 item 5; `agents` per
 *  brief-009 item 3 (rename) with `members` kept as deprecated alias. */
export const EXPECTED_TOOL_BASE_NAMES = [
  'msg_send',
  'msg_ls',
  'msg_read',
  'msg_archive',
  'msg_thread',
  'members',
  'agents',
  'resource_add',
  'resource_ls',
  'resource_read',
  'resource_remove',
] as const;

/** brief-005-phase0 §3: every tool dual-registers under `coord_*`
 *  (legacy) AND `st_*` (new). The Phase-1 set is both prefixes for
 *  every base name. Phase 5 drops the `coord_*` variants. */
export const EXPECTED_TOOL_NAMES = [
  ...EXPECTED_TOOL_BASE_NAMES.map((n) => `coord_${n}` as const),
  ...EXPECTED_TOOL_BASE_NAMES.map((n) => `st_${n}` as const),
] as const;

/** Channel-mode tool set: non-channel set + msg_reply (dual-prefixed). */
export const EXPECTED_TOOL_NAMES_CHANNEL = [
  ...EXPECTED_TOOL_NAMES,
  'coord_msg_reply',
  'st_msg_reply',
] as const;

export type ToolName = (typeof EXPECTED_TOOL_NAMES_CHANNEL)[number];

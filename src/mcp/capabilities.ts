// mcp/capabilities.ts — canonical name/version/capabilities for the
// `st mcp` server. Centralized here so lifecycle / channel tests can
// snapshot the exact same options the server is built from.

import type { ServerOptions } from '@modelcontextprotocol/sdk/server/index.js';
import type { Implementation } from '@modelcontextprotocol/sdk/types.js';

/**
 * Default server identity — preserved for back-compat with imports
 * that pre-date brief-005-phase0. New code should call
 * {@link buildServerInfo} so the server announces under whichever
 * name (`st` / `st`) the CLI was invoked as.
 */
export const SERVER_INFO: Implementation = {
  name: 'st',
  // Tracks the package version. We don't read package.json here to keep
  // the MCP module side-effect free; bump on every st release.
  // 0.1.0 — brief-022 ships unknown-state + boot ritual + offline-on-
  //         shutdown. Real surface change visible to MCP hosts.
  // 0.2.0 — brief-023 (status-file mtime refresh) + brief-024
  //         (`journal/` folder + CLI verbs). MCP surface unchanged
  //         (Phase 8 journal tools deferred), but the data-model
  //         layout grows so peers see a new folder shape.
  // 0.2.1 — brief-028 (lib API: st.members / st.overview /
  //         st.createIdentity + type re-exports). MCP surface
  //         unchanged; library-only addition.
  // 0.2.2 — brief-029 (`away` State, fifth settable). Additive
  //         enum extension; st_members filter enum picks it up
  //         via STATES, no schema-bump beyond the new value.
  // 0.3.0 — brief-030 (tidy-check tick): MCP server emits
  //         synthetic notifications/claude/channel frames from
  //         st-system when drift is detected (stale inbox,
  //         untouched doing-task, journal lag). Real feature,
  //         hence the minor bump.
  // 0.4.0 — brief-009 phase 1: tasks/ surface removed across CLI,
  //         SDK, MCP onboarding, and tidy-check.
  // 0.5.0 — brief-009 phase 2: journal/ surface removed (same
  //         treatment). Tidy-check is now inbox-staleness only.
  // 0.6.0 — brief-009 phase 5: resources/ surface added (annotated
  //         URLs per identity). 4 new MCP tools, dual-prefixed.
  // 0.7.0 — brief-009 item 3: identity → agent rename across SDK
  //         types, MCP tool names (st_members → st_agents
  //         keeping st_members as deprecated alias), CLI verb
  //         (members → agents, members deprecated alias), and env
  //         vars (ST_AGENT preferred → ST_IDENTITY → ST_IDENTITY).
  // 0.8.0 — brief-009 item 4: SDK parity gap-fills (st.archive +
  //         st.archiveTrim gain `withAttachments`; new
  //         st.lsOrphans and st.ding on the handle). No CLI
  //         or MCP surface change.
  // 0.8.1 — `st mcp` startup falls back to a throwaway
  //         `anon-<rand6>` agent when no ST_AGENT/ST_IDENTITY/
  //         ST_IDENTITY is set, instead of hard-exiting. Unblocks
  //         MCP hosts (Codex etc.) that spawn the server without
  //         identity env. One-line stderr warning + lazy-create the
  //         anon agent's inbox/archive folders.
  // 0.9.0 — brief-016: `smalltalk launch <claude|codex>` verb —
  //         one-command harness bootstrap onto smalltalk (identity,
  //         .mcp.json, session-id, pty registration when available,
  //         `st ding` sidecar for codex, `ollama launch` route
  //         for GLM-backed launches via --model).
  // 0.9.1 — brief-020 (HB-4): channel-watcher wake reliability —
  //         adds a polling backstop that catches inbox files
  //         chokidar's FSEvents backend may have silently dropped,
  //         so idle Claude Code agents don't wedge on unnotified
  //         deliveries. Server surface unchanged; operators can
  //         opt-in to stderr instrumentation via
  //         ST_CHANNEL_DEBUG=1.
  version: '0.9.1',
};

/**
 * Build a server-info record that announces under the canonical name.
 * Post-st-cutover: `st` is the only canonical name — `smalltalk`
 * resolves to `st` via {@link canonicalServerName}.
 */
export function buildServerInfo(name: 'st'): Implementation {
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
 * full boot ritual every connected agent runs so the operator's
 * visibility surface — status files + inbox flow — stays honest.
 *
 * Load-bearing substrings (asserted by tests/unit/channel-instructions
 * regression guard): `available`, `st status`, `st_msg_ls`,
 * `st_msg_read`, `st_msg_archive`, `st_msg_reply`, `st_agents`,
 * `<channel source="st"`.
 *
 * **Bus-contract note (post `st launch` deletion):** MCP agents
 * receive THIS blurb via the transport's `instructions:` field.
 * Ding-mode agents receive an analogous DING-BUS.md installed by
 * their launcher (convoy owns that surface now — see convoy's
 * DING-BUS template). When the *shared* contract changes (new
 * tools, new conventions, boot-ritual updates), keep this blurb in
 * sync with convoy's vendored template so MCP agents and ding-mode
 * agents behave identically for the same protocol event.
 */
export const CHANNEL_INSTRUCTIONS = [
  'You are connected to smalltalk. This is the operator\'s visibility layer: they look at status files and inbox/archive to understand what every agent is doing. Maintain those records.',
  '',
  'On first opportunity in a fresh context (cold start or /clear):',
  '  1. Write `available` to your status file. Shell out: `st status <your-identity> --set available`. Use $ST_AGENT.',
  '  2. Drain your inbox backlog. Call `st_msg_ls` to enumerate filenames, then for each one `st_msg_read` it, `st_msg_reply` if appropriate, and `st_msg_archive` to clear. Don\'t leave inbox items un-addressed.',
  '  3. Check `st_agents` (with `enrich: true`) to see who\'s around and whether any peers are waiting on you.',
  '',
  'As channel notifications arrive: new peer messages appear in your context as `<channel source="st" from="<sender>">…</channel>` blocks. For each one: `st_msg_read` the cited filename, `st_msg_reply` with `thread: <messageFilename>` and `body: <your reply>` if a response is warranted, then `st_msg_archive` to clear. Don\'t let inbox accumulate.',
  '',
  'Smalltalk threads stay on smalltalk. A thread that originated from a channel notification or an inbox message is conversed *only* via `st_msg_send` / `st_msg_reply` — questions, clarifications, blockers, "I think I\'m done" signals, follow-up thoughts, all of it. By default, your pty REPL is unattended — there is no human reading what you print to your own screen. Your smalltalk correspondent is your interlocutor for the thread; they will relay anything that matters to the user. If you would otherwise pause to ask "should I do X?" at your REPL, send it via `st_msg_reply` instead. The only time it\'s right to address the REPL is when a human directly typed there.',
  '',
  'Tools you have via MCP: `st_msg_send`, `st_msg_reply`, `st_msg_ls`, `st_msg_read`, `st_msg_archive`, `st_msg_thread`, `st_agents`. For status, shell out to `st status` — no MCP tool for it yet.',
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
 *  Post-cutover: `st_*` only. The historical `st_*` dual-register
 *  and the deprecated `members` alias have both been removed —
 *  `st_agents` is the canonical name. */
export const EXPECTED_TOOL_BASE_NAMES = [
  'msg_send',
  'msg_ls',
  'msg_read',
  'msg_archive',
  'msg_thread',
  'agents',
  'resource_add',
  'resource_ls',
  'resource_read',
  'resource_remove',
  'context_read',
  'context_write',
  'context_append',
] as const;

/** Post-st-cutover: every tool registers under `st_*` only.
 *  The historical `st_*` alias set (via dual-register) has been
 *  removed — one canonical name each. */
export const EXPECTED_TOOL_NAMES = [
  ...EXPECTED_TOOL_BASE_NAMES.map((n) => `st_${n}` as const),
] as const;

/** Channel-mode tool set: non-channel set + msg_reply. */
export const EXPECTED_TOOL_NAMES_CHANNEL = [
  ...EXPECTED_TOOL_NAMES,
  'st_msg_reply',
] as const;

export type ToolName = (typeof EXPECTED_TOOL_NAMES_CHANNEL)[number];

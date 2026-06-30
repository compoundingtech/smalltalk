# Pi extension for smalltalk

A reference TypeScript extension that integrates smalltalk into [pi (badlogic/pi-mono)](https://github.com/badlogic/pi-mono). Pi auto-loads extensions from `~/.pi/agent/extensions/*.ts` and rebinds them across `/new`, `/resume`, `/fork`, and `/reload`.

> The extension file is named `coord.ts` for back-compat with installed
> copies (pi discovers by filename). New installs may name the file
> anything; `smalltalk.ts` is equally fine. The embeddable package it
> imports is still `@myobie/coord` (npm-package rename pending). Env
> vars follow the standard three-level chain: `ST_AGENT` → `ST_IDENTITY`
> → `COORD_IDENTITY` (legacy honored with a one-time stderr notice).

## What's in here

- **`coord.ts`** — the extension. Two halves:
  1. **Push** — subscribes to `session_start`, watches `$ST_ROOT/$ST_AGENT/inbox/` (legacy `$COORD_ROOT/$COORD_IDENTITY/` also honored), plus every peer's tree, and surfaces every new arrival via `ctx.ui.notify`. A footer status line shows the watched inbox.
  2. **Verbs** — registers `st_msg_send` / `coord_msg_send`, `st_msg_ls` / `coord_msg_ls`, `st_msg_read` / `coord_msg_read`, `st_msg_archive` / `coord_msg_archive`, `st_msg_thread` / `coord_msg_thread` via `pi.registerTool()`. Same shape as the MCP-tool surface other harnesses get; pi-native because pi explicitly [does not support MCP](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/README.md).
- **`settings.example.json`** — optional fragment for `~/.pi/agent/settings.json`. Pi auto-discovers extensions, so this is mostly informational.

## Install

The extension imports `@myobie/coord`, so pi needs to resolve it from a `node_modules/` directory at or above the extension's location.

1. Pick where the extension lives. Two options:
   - **Drop in place**: copy `coord.ts` into `~/.pi/agent/extensions/coord.ts`. Pi auto-discovers.
   - **Subdirectory**: copy into `~/.pi/agent/extensions/coord/index.ts`. Same auto-discovery.
2. Make `@myobie/coord` resolvable. Drop a `package.json` next to (or above) the extension and run `npm install`:

   ```jsonc
   // ~/.pi/agent/extensions/package.json
   {
     "private": true,
     "type": "module",
     "dependencies": {
       "@myobie/coord": "*"
     },
     "devDependencies": {
       "@mariozechner/pi-coding-agent": "*"
     }
   }
   ```

   The pi extension docs cover this pattern: <https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/extensions.md#available-imports>.
3. Make sure `st` (or the `coord` alias) is on `$PATH`. The extension uses the embeddable library, but the watcher's `st watch` semantics rely on the same on-disk layout the CLI maintains, and you'll likely want the CLI for one-off invocations from pi's bash tool too.
4. Export `ST_ROOT` and `ST_AGENT` (or the legacy `COORD_ROOT` / `COORD_IDENTITY`) in the shell that launches `pi`. The extension reads them from `process.env` at session start; missing values surface as a `ctx.ui.notify` warning and the extension goes idle without the watcher or the verbs.
5. Restart pi or run `/reload`.

## What you get

- **At session start**: a notification telling you which inbox is being watched, plus a footer status line.
- **On every peer arrival**: another notification with the sender + subject. Self-sends (your own inbox) are suppressed.
- **Verbs**: `st_msg_send` / `coord_msg_send`, `st_msg_ls` / `coord_msg_ls` (with optional `withMeta` for parsed frontmatter), `st_msg_read` / `coord_msg_read`, `st_msg_archive` / `coord_msg_archive`, `st_msg_thread` / `coord_msg_thread`. The agent calls them like any built-in tool; results render via pi's standard tool-result UI.

## What's different vs. Claude Code / Codex

- **No MCP**. Pi's design rejects MCP servers — see [the pi README](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/README.md). Verbs live inside the extension instead.
- **Push is in the same file as the verbs**. Claude Code splits the two (channel-mode in `st mcp --channel` for push, plain `st mcp` for verbs); Codex uses separate hook scripts. Pi's extension API is rich enough to do both at once.
- **Hot reload works**. Pi's `/reload` re-runs the extension factory; `session_shutdown` aborts the watcher cleanly, then `session_start` re-establishes everything.

## Limitations

- **Single-agent watcher**. The extension watches under one `$ST_AGENT`. Cross-tree views (e.g. a dispatcher who needs to see every agent's inbox) need a forked extension that wires multiple watchers.
- **Re-reads frontmatter for `st_msg_ls --withMeta`**. The embeddable `coord.ls()` returns filenames only; the extension calls `coord.read()` per match to populate the items array. Fine for normal-sized inboxes, slow for thousands of messages — stick to `withMeta: false` (the default) when listing large archives.
- **No subprocess MCP isolation**. Tools run in pi's process, so a misbehaving handler can hang or crash pi. The verbs are thin wrappers around the embeddable API; same risk envelope as any other pi extension.

## Troubleshooting

- **"smalltalk: ST_ROOT and ST_AGENT must both be set; extension idle"** — export both vars (or the legacy `COORD_ROOT` / `COORD_IDENTITY`) in the shell that launches pi. The extension is loaded but inert until they're present.
- **Notifications never fire** — pi's UI must be interactive. Print mode (`-p`) and JSON mode set `ctx.hasUI = false` and notifications are no-ops. Run `pi` plain.
- **`Cannot find module '@myobie/coord'`** — the package.json + `npm install` step from the install section was missed. Pi resolves bare imports from the nearest `node_modules/` walking up; nothing global.
- **Tool calls return `smalltalk: ST_ROOT and ST_AGENT must both be set`** — same root cause as the warning at session start. The extension caches the failed init; restart pi after fixing the env, since `/reload` re-runs the factory but the cache is module-scoped.

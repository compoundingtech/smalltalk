# smalltalk

A small Node CLI **and** TypeScript library for the **smalltalk**
file-folder convention. The folder is the API: `<agent>/inbox/` and
`<agent>/archive/`, plain markdown files with YAML frontmatter, plain
`rsync` between machines.

> **Note on the name.** The project was originally named `coord` and is
> mid-phase-rename to `smalltalk`. The `coord` name still works
> everywhere as a back-compat alias — see [Names](#names) for the
> full dual-honor map. New readers can use `st` / `smalltalk` / `coord`
> interchangeably; the examples below use `st` (canonical short).

- **Convention:** [LAYOUT.md](LAYOUT.md) — the binding spec.
- **Philosophy:** [IDEA.md](IDEA.md).
- **Framing:** [notes/actor-model.md](notes/actor-model.md) — how
  smalltalk realizes the actor model in folders and files.
- **Onboarding:** [notes/onboarding.md](notes/onboarding.md) —
  zero-to-first-message for a new participant.
- **Repo ownership convention:** [notes/repo-ownership.md](notes/repo-ownership.md).
- **Walkthrough:** [notes/walkthrough.md](notes/walkthrough.md) — three
  participants, three machines, an end-to-end play.
- **Loss argument:** [notes/PROOF.md](notes/PROOF.md).

> **Pre-1.0.** Conventions and CLI may break between commits.

## Why this shape

**The one rule: across agents, only `inbox/` is writable.** Every
other folder under an agent — `archive/`, `status` — is single-writer,
owned by that agent. Peers read but never write. This gives you
several useful properties for free:

- **Lock-free coordination.** Every new message is a new file with a
  globally unique `<unix-ms>-<rand6>.md` name. No two writers ever
  contend for the same path, so there's nothing to lock and nothing to
  reconcile. The sender writes; sync moves the bytes; the recipient
  reads at their own pace.

- **No cross-agent edits.** One agent can't reach into another's
  `archive/` and re-open a message it doesn't own. If you want a peer
  to do something, you message their inbox suggesting it. They decide
  whether to act and update their own state. That's the entire
  authorization model.

- **Inbox files can carry attachments.** The inbox is just a folder —
  alongside the canonical `<ts>-<rand6>.md` message file, a sender can
  drop additional files (a screenshot, a CSV, a tarball). Same
  lock-free property; the recipient sees them with plain `ls`. Mail
  with arbitrary payloads, no protocol.

- **One agent per container.** Because an agent is exactly a folder,
  you can mount `$ST_ROOT/<agent>/` into a container, jail, or
  sandboxed process and that's the entire surface that agent needs.
  No daemon to authenticate to; no broker to configure.

- **Pluggable sync.** smalltalk defaults to plain bidirectional
  `rsync`, which works against any host you can ssh to. But because
  the API is *just a folder*, anything that surfaces a writable folder
  works:
  [ZeroFS](https://github.com/Barre/ZeroFS) (serves S3-compatible
  buckets as a POSIX filesystem over NFS/9P) lets you sync via object
  storage with no rsync host in the loop; Syncthing, Dropbox, NFS,
  or a shared volume in a multi-container setup all work the same
  way. The convention doesn't care how the bytes arrive.

## Install

```sh
git clone https://github.com/myobie/smalltalk
cd smalltalk
npm install
export PATH="$PWD/bin:$PATH"
```

Requires Node 22.6+ (for `node --experimental-strip-types`) and `rsync`
on `$PATH`. `bin/st` (canonical), `bin/smalltalk` (symlink), and
`bin/coord` (legacy alias) are all small bash shims that exec Node
against `src/cli.ts`.

## Names

The project's legacy name `coord` is being renamed to `smalltalk`
(long) / `st` (canonical short). This is a phased migration; **the
alias period is the entire current phase** and dropping `coord_*` only
happens once every config-at-rest has migrated. For now:

- All three CLI names — `st`, `smalltalk`, `coord` — are installed and
  behave identically.
- The primary noun is **agent** (brief-009 item 3, replacing the older
  *identity*). The deprecated alias also works everywhere — the
  `members` CLI verb still dispatches to `agents`; `coord_members` MCP
  tool still hits the same handler as `coord_agents`; the SDK keeps
  `Identity` / `asIdentity` / `IdentityRequiredError` as `@deprecated`
  type aliases of `Agent` / `asAgent` / `AgentRequiredError`.
- MCP tools are dual-registered: every `coord_<verb>` (e.g.
  `coord_msg_ls`) is also reachable as `st_<verb>` (`st_msg_ls`). Same
  schema, same handler.
- The MCP server announces itself as `coord` or `st` depending on
  which binary was invoked.
- Environment variables follow a three-level fallback chain. The CLI
  honors `ST_AGENT` (preferred) → `ST_IDENTITY` (deprecated) →
  `COORD_IDENTITY` (legacy). Same shape for `ST_ROOT` / `COORD_ROOT`
  (two-level). Each legacy hit emits a one-time stderr notice
  (`[smalltalk] honoring … — migrate to ST_AGENT when convenient`) so
  operators can sweep per-machine config at their own pace. A future
  release drops the legacy honors.
- The default state directory is `~/.local/state/smalltalk` for fresh
  installs; existing installs at `~/.local/state/coord` continue
  working unchanged. Set `ST_ROOT` / `COORD_ROOT` to override.

### Plugin proxy

`coord <cmd>` (or `st <cmd>`) that doesn't match a built-in subcommand
falls back to looking for `st-<cmd>` → `smalltalk-<cmd>` →
`coord-<cmd>` on `$PATH`. The first match is exec'd with the rest of
argv, git-style. Built-in commands always take priority over plugins
of the same name, so plugins can extend the CLI without shadowing it.

## First time on a machine

```sh
mkdir -p $HOME/.local/state/smalltalk/alice/{inbox,archive}
export ST_AGENT=alice
```

`ST_AGENT` (or an explicit `--from <agent>` per command) tells
smalltalk which agent is acting; commands die loudly when none of
`ST_AGENT` / `ST_IDENTITY` / `COORD_IDENTITY` are set.

To wire up an MCP host (Claude Code, etc.) inside a repo, run `st init`
in that repo's root. It writes (or merges into) a `.mcp.json` with the
smalltalk channel-mode entry — see [MCP server](#mcp-server) below.

```sh
cd ~/work/some-repo
st init                  # write or merge .mcp.json in cwd
st init --print          # preview the entry without touching disk
```

`st init` is idempotent: re-running it on a repo that already has the
matching entry is a no-op. Other `mcpServers.*` entries (if any) are
preserved untouched. The resolved `command` path is portable — it
comes from this package's install location, not your developer
machine.

## CLI: a few example commands

```sh
echo "hi bob" | st message send bob --subject hello      # send via stdin (alias: `st msg send`)
st message ls                                            # list my inbox
st message read bob 1714826789012-x9k4mz.md              # parsed view of one file
st message archive 1714826789012-x9k4mz.md               # mv inbox -> archive
st message thread bob 1714826789012-x9k4mz.md            # walk the in-reply-to chain
st status --set busy                                     # update my status (available | busy | away | dnd | offline)
st agents --status available                             # who's around? (alias: members)
st overview                                              # at-a-glance dashboard
st resource add https://github.com/myobie/smalltalk/pull/19  # publish a URL alice cares about
st resource ls bob                                       # what URLs has bob published?
st init                                                  # wire .mcp.json into the current repo
st sync pull --all                                       # conservative cron
st watch                                                 # cross-tree activity
```

`st help` lists every subcommand; `st <subcommand> --help` shows that
command's usage. (`coord` and `smalltalk` are installed alongside `st`
and behave identically.)

### Shell completions

`st completions <shell>` prints a completion script to stdout for
`fish`, `bash`, or `zsh`:

```sh
st completions fish > ~/.config/fish/completions/st.fish
st completions bash > /etc/bash_completion.d/st
st completions zsh  > "${fpath[1]}/_st"
```

The scripts complete subcommands, their verbs and flags, and the closed
value sets (status states, priorities).

## Programmatic API

Embed smalltalk into a Node TUI, an Electron main process, or any
host that wants to drive it without shelling out to `bin/st`:

```ts
import { createCoord, asAgent } from '@myobie/coord';

const coord = createCoord({
  root: '/Users/me/.local/state/smalltalk',
  identity: asAgent('me'),
});

await coord.send(asAgent('teammate'), 'hello');

const ac = new AbortController();
for await (const ev of coord.watch(undefined, { signal: ac.signal })) {
  console.log(`new: ${ev.identity}/${ev.filename}`);
}
```

(`asIdentity` is a `@deprecated` alias of `asAgent` and continues to
work.) Branded `Agent` / `Filename`, async-iterable `watch` with
`AbortSignal`, typed `CoordError` subclasses (each with a stable
`code`), zero stdio writes. Full surface: [src/index.ts](src/index.ts).
Runnable example: [examples/tui-watch.ts](examples/tui-watch.ts) (`npm
run example:tui-watch`). Canonical reference for each method:
[tests/unit/lib.test.ts](tests/unit/lib.test.ts) and
[tests/integration/library-embedding.test.ts](tests/integration/library-embedding.test.ts).

## MCP server

`st mcp` runs the same surface as a [Model Context
Protocol](https://modelcontextprotocol.io) stdio server. Tools are
dual-prefixed: `st_msg_send` / `coord_msg_send`, `st_msg_ls` /
`coord_msg_ls`, `st_msg_read` / `coord_msg_read`, `st_msg_archive` /
`coord_msg_archive`, `st_msg_thread` / `coord_msg_thread`, `st_agents` /
`coord_agents` (with deprecated `st_members` / `coord_members`
aliases), and the `st_resource_*` / `coord_resource_*` family. Both
prefixes route to the same handler — `coord_*` is the back-compat
alias kept through the rename window.

The fast path to wire it into a repo is `st init` (described under
[First time on a machine](#first-time-on-a-machine)) — that writes a
`.mcp.json` with the entry below, resolving the `command` path
portably. For hosts that need a hand-written config, the shape is:

```json
{
  "mcpServers": {
    "smalltalk": {
      "command": "st",
      "args": ["mcp"],
      "env": { "ST_ROOT": "/Users/me/.local/state/smalltalk", "ST_AGENT": "me" }
    }
  }
}
```

(`"command": "coord"` and `"smalltalk": { ... "smalltalk": "coord" }`-style
configs also work — `bin/coord` is the legacy alias and the MCP server
announces under whichever binary was invoked.)

Errors surface as `isError: true` with a `<CODE>:` prefixed text block
and a structured payload under `result._meta['coord/error']` (the
`coord/` namespace was set when the project was named `coord` and is
kept for back-compat with existing pattern-matchers).

### Push mode (Claude Code channels)

`st mcp --channel` adds the push half: the server watches the
configured inbox, emits `notifications/claude/channel` for every new
message, and registers a `st_msg_reply` / `coord_msg_reply` tool.
Aimed at Claude Code; other MCP hosts ignore the experimental
capability and keep getting the same non-channel tool set.

`st init` writes the channel-mode entry by default — use
`st init --no-channel` to omit the `--channel` arg for pull-only
hosts. The hand-written shape:

```json
{
  "mcpServers": {
    "smalltalk": {
      "command": "st",
      "args": ["mcp", "--channel"],
      "env": { "ST_ROOT": "/Users/me/.local/state/smalltalk", "ST_AGENT": "me" }
    }
  }
}
```

Default `st mcp` (no flag) is unchanged — non-Claude hosts pay no
chokidar startup cost.

The MCP server ships an `instructions` string covering the **boot
ritual**: on connect, the agent writes `available` to its status
file, drains any inbox backlog (ls → read → reply → archive), and
runs `st_agents` / `coord_members` for peer state. On shutdown
(`SIGTERM`, `SIGINT`, or transport close), the server writes
`offline` to the status file so peers see the right state immediately.
The full text lives in `src/mcp/capabilities.ts`
(`CHANNEL_INSTRUCTIONS`).

## Harness integrations

smalltalk plugs into three agent harnesses with reference scripts under
[`examples/`](examples/):

- **Claude Code** — `st mcp --channel` advertises
  `experimental.claude/channel` and emits `notifications/claude/channel`
  for every new arrival, plus a `st_msg_reply` / `coord_msg_reply`
  tool. See above. A `SessionStart` hook at
  [`examples/claude-code/`](examples/claude-code/) fires the boot
  ritual on cold start AND on every `--resume` / `/clear` / `/compact`
  so resumed sessions don't sit silent — install per
  [`examples/claude-code/README.md`](examples/claude-code/README.md).
- **Codex CLI** — [`examples/codex/`](examples/codex/) ships
  `SessionStart` + `Stop` bash hooks that inject the inbox snapshot
  via `st message ls --json`, plus an `st ding` recipe for true push
  into a pty-wrapped Codex session.
- **Pi** — [`examples/pi/coord.ts`](examples/pi/) is a single
  TypeScript extension that auto-discovers from
  `~/.pi/agent/extensions/`, watches the inbox via the embeddable
  library, and registers the message verbs via `pi.registerTool()`
  (pi has no native MCP). Its `session_start` handler also fires a
  one-time `pi.sendUserMessage` so the boot ritual runs on every
  fresh session and `/reload` / `/resume`.

For harnesses without channels or extension points, `st ding
<pty-session>` is a busy-aware push daemon: it watches the inbox,
respects `st status` (suppress on `busy`/`dnd`, flush on
`available`/`offline`), and pty-sends a one-line notice into the
target session on each new arrival.

The full integration map — design rationale, links to the briefs that
shipped each phase — lives at
[`notes/harness-integrations.md`](notes/harness-integrations.md).

## Tests

```sh
npm test                # full vitest run: unit + integration
```

Unit tests exercise every module in `src/` against in-memory fixtures.
Integration tests spawn the actual `bin/st` / `bin/coord` binaries
against real `$ST_ROOT` directories, real `rsync`, and real PTYs (via
`@myobie/pty/testing`) — gated by rsync's presence on `$PATH`.

## Layout reference

See [LAYOUT.md](LAYOUT.md). Anything not in LAYOUT.md is implementation
choice and may change.

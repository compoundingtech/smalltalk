# coord

A small Node CLI **and** TypeScript library for the **coord** file-folder
convention. The folder is the API: `<identity>/inbox/`, `<identity>/archive/`,
plus the optional `<identity>/journal/` log, plain markdown files with YAML
frontmatter, plain `rsync` between machines.

- **Convention:** [LAYOUT.md](LAYOUT.md) — the binding spec.
- **Philosophy:** [IDEA.md](IDEA.md).
- **Walkthrough:** [notes/walkthrough.md](notes/walkthrough.md) — three
  participants, three machines, an end-to-end play.
- **Loss argument:** [notes/PROOF.md](notes/PROOF.md).

> **Pre-1.0.** Conventions and CLI may break between commits.

## Why this shape

**The one rule: across identities, only `inbox/` is writable.** Every
other folder under an identity — `archive/`, `journal/`, `status` — is
single-writer, owned by that identity. Peers read but never write. This
gives you several useful properties for free:

- **Lock-free coordination.** Every new message is a new file with a
  globally unique `<unix-ms>-<rand6>.md` name. No two writers ever
  contend for the same path, so there's nothing to lock and nothing to
  reconcile. The sender writes; sync moves the bytes; the recipient
  reads at their own pace.

- **No cross-identity edits.** One identity can't reach into another's
  `journal/` and edit an entry it doesn't own. If you want a peer to do
  something, you message their inbox suggesting it. They decide whether
  to act and update their own state. That's the entire authorization
  model.

- **Inbox files can carry attachments.** The inbox is just a folder —
  alongside the canonical `<ts>-<rand6>.md` message file, a sender can
  drop additional files (a screenshot, a CSV, a tarball). Same
  lock-free property; the recipient sees them with plain `ls`. Mail
  with arbitrary payloads, no protocol.

- **One identity per container.** Because an identity is exactly a
  folder, you can mount `$COORD_ROOT/<identity>/` into a container,
  jail, or sandboxed process and that's the entire surface that
  identity needs. No daemon to authenticate to; no broker to
  configure.

- **Pluggable sync.** `coord` defaults to plain bidirectional `rsync`,
  which works against any host you can ssh to. But because the API is
  *just a folder*, anything that surfaces a writable folder works:
  [ZeroFS](https://github.com/Barre/ZeroFS) (serves S3-compatible
  buckets as a POSIX filesystem over NFS/9P) lets you sync via object
  storage with no rsync host in the loop; Syncthing, Dropbox, NFS,
  or a shared volume in a multi-container setup all work the same
  way. The convention doesn't care how the bytes arrive.

## Install

```sh
git clone https://github.com/myobie/coord
cd coord
npm install
export PATH="$PWD/bin:$PATH"
```

Requires Node 22.6+ (for `node --experimental-strip-types`) and `rsync`
on `$PATH`. `bin/st` (canonical), `bin/smalltalk` (symlink), and
`bin/coord` (legacy alias) are all small bash shims that exec Node
against `src/cli.ts`.

## Names

`coord` is being renamed to `smalltalk` (long) / `st` (canonical short).
This is a phased migration; **the alias period is the entire current
phase** and dropping `coord_*` only happens once every config-at-rest
has migrated. For now:

- All three CLI names — `st`, `smalltalk`, `coord` — are installed and
  behave identically.
- MCP tools are dual-registered: every `coord_<verb>` (e.g.
  `coord_msg_ls`) is also reachable as `st_<verb>` (`st_msg_ls`). Same
  schema, same handler.
- The MCP server announces itself as `coord` or `st` depending on
  which binary was invoked.
- Environment variables follow the same pattern. The CLI honors both
  `ST_IDENTITY` (preferred) and `COORD_IDENTITY` (legacy), and the
  same for `ST_ROOT` / `COORD_ROOT`. When only the legacy name is
  set, a one-time stderr notice ("`[smalltalk] honoring COORD_…`")
  flags that the config can migrate when convenient.
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
mkdir -p $HOME/.local/state/coord/alice/{inbox,archive}
export COORD_IDENTITY=alice
```

`COORD_IDENTITY` (or an explicit `--from <id>` per command) tells coord
which identity is acting; commands die loudly when neither is set.

To wire up an MCP host (Claude Code, etc.) inside a repo, run `coord init`
in that repo's root. It writes (or merges into) a `.mcp.json` with the
coord channel-mode entry — see [MCP server](#mcp-server) below.

```sh
cd ~/work/some-repo
coord init                  # write or merge .mcp.json in cwd
coord init --print          # preview the entry without touching disk
```

`coord init` is idempotent: re-running it on a repo that already has
the matching entry is a no-op. Other `mcpServers.*` entries (if any)
are preserved untouched. The resolved `command` path is portable — it
comes from this package's install location, not your developer
machine.

## CLI: a few example commands

```sh
echo "hi bob" | coord message send bob --subject hello      # send via stdin (alias: `coord msg send`)
coord message ls                                            # list my inbox
coord message read bob 1714826789012-x9k4mz.md              # parsed view of one file
coord message archive 1714826789012-x9k4mz.md               # mv inbox -> archive
coord message thread bob 1714826789012-x9k4mz.md            # walk the in-reply-to chain
coord status --set busy                                     # update my status (available | busy | away | dnd | offline)
coord journal new "shipped brief-024" --tag layout          # terse work-log entry
coord journal tail worker-claude -n 5                       # follow a peer's narrative
coord members --status available                            # who's around?
coord overview                                              # at-a-glance dashboard
coord init                                                  # wire .mcp.json into the current repo
coord sync pull --all                                       # conservative cron
coord watch                                                 # cross-tree activity
```

`coord help` lists every subcommand; `coord <subcommand> --help` shows
that command's usage.

### Shell completions

`coord completions <shell>` prints a completion script to stdout for
`fish`, `bash`, or `zsh`:

```sh
coord completions fish > ~/.config/fish/completions/coord.fish
coord completions bash > /etc/bash_completion.d/coord
coord completions zsh  > "${fpath[1]}/_coord"
```

The scripts complete subcommands, their verbs and flags, and the closed
value sets (status states, priorities). The fish script also
disambiguates the reused verbs (`ls`, `new`) across the `message` /
`journal` groups.

## Programmatic API

Embed coord into a Node TUI, an Electron main process, or any host that
wants to drive coord without shelling out to `bin/coord`:

```ts
import { createCoord, asIdentity } from '@myobie/coord';

const coord = createCoord({
  root: '/Users/me/.local/state/coord',
  identity: asIdentity('me'),
});

await coord.send(asIdentity('teammate'), 'hello');

const ac = new AbortController();
for await (const ev of coord.watch(undefined, { signal: ac.signal })) {
  console.log(`new: ${ev.identity}/${ev.filename}`);
}
```

Branded `Identity` / `Filename`, async-iterable `watch` with
`AbortSignal`, typed `CoordError` subclasses (each with a stable
`code`), zero stdio writes. Full surface: [src/index.ts](src/index.ts).
Runnable example: [examples/tui-watch.ts](examples/tui-watch.ts) (`npm
run example:tui-watch`). Canonical reference for each method:
[tests/unit/lib.test.ts](tests/unit/lib.test.ts) and
[tests/integration/library-embedding.test.ts](tests/integration/library-embedding.test.ts).

## MCP server

`coord mcp` runs the same surface as a [Model Context
Protocol](https://modelcontextprotocol.io) stdio server. Tools:
`coord_msg_send`, `coord_msg_ls`, `coord_msg_read`, `coord_msg_archive`,
`coord_msg_thread`, `coord_members`.

The fast path to wire it into a repo is `coord init` (described under
[First time on a machine](#first-time-on-a-machine)) — that writes a
`.mcp.json` with the entry below, resolving the `command` path
portably. For hosts that need a hand-written config, the shape is:

```json
{
  "mcpServers": {
    "coord": {
      "command": "coord",
      "args": ["mcp"],
      "env": { "COORD_ROOT": "/Users/me/.local/state/coord", "COORD_IDENTITY": "me" }
    }
  }
}
```

Errors surface as `isError: true` with a `<CODE>:` prefixed text block
and a structured payload under `result._meta['coord/error']` (namespaced
passthrough — schema-free).

### Push mode (Claude Code channels)

`coord mcp --channel` adds the push half: the server watches the
configured inbox, emits `notifications/claude/channel` for every new
message, and registers a `coord_msg_reply` tool. Aimed at Claude Code; other
MCP hosts ignore the experimental capability and keep getting the same
non-channel tool set.

`coord init` writes the channel-mode entry by default — use
`coord init --no-channel` to omit the `--channel` arg for pull-only
hosts. The hand-written shape:

```json
{
  "mcpServers": {
    "coord": {
      "command": "coord",
      "args": ["mcp", "--channel"],
      "env": { "COORD_ROOT": "/Users/me/.local/state/coord", "COORD_IDENTITY": "me" }
    }
  }
}
```

Default `coord mcp` (no flag) is unchanged — non-Claude hosts pay no
chokidar startup cost.

The MCP server ships an `instructions` string covering the **boot
ritual**: on connect, the agent writes `available` to its status
file, drains any inbox backlog (ls → read → reply → archive), and
runs `coord_members` for peer state. As non-trivial progress happens,
it drops `coord journal new` entries so peers can follow what shipped.
On shutdown (`SIGTERM`, `SIGINT`, or transport close), the server
writes `offline` to the status file so peers see the right state
immediately. The full text lives in `src/mcp/capabilities.ts`
(`CHANNEL_INSTRUCTIONS`).

## Harness integrations

Coord plugs into three agent harnesses with reference scripts under
[`examples/`](examples/):

- **Claude Code** — `coord mcp --channel` advertises
  `experimental.claude/channel` and emits `notifications/claude/channel`
  for every new arrival, plus a `coord_msg_reply` tool. See above.
  A `SessionStart` hook at
  [`examples/claude-code/`](examples/claude-code/) fires the boot
  ritual on cold start AND on every `--resume` / `/clear` / `/compact`
  so resumed sessions don't sit silent — install per
  [`examples/claude-code/README.md`](examples/claude-code/README.md).
- **Codex CLI** — [`examples/codex/`](examples/codex/) ships
  `SessionStart` + `Stop` bash hooks that inject the inbox snapshot
  via `coord message ls --json`, plus a `coord ding` recipe for true push
  into a pty-wrapped Codex session.
- **Pi** — [`examples/pi/coord.ts`](examples/pi/) is a single
  TypeScript extension that auto-discovers from
  `~/.pi/agent/extensions/`, watches the inbox via the embeddable
  library, and registers the five coord verbs via
  `pi.registerTool()` (pi has no native MCP). Its `session_start`
  handler also fires a one-time `pi.sendUserMessage` so the boot
  ritual runs on every fresh session and `/reload` / `/resume`.

For harnesses without channels or extension points, `coord ding
<pty-session>` is a busy-aware push daemon: it watches the inbox,
respects `coord status` (suppress on `busy`/`dnd`, flush on
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
Integration tests spawn the actual `bin/coord` binary against real
`$COORD_ROOT` directories, real `rsync`, and real PTYs (via
`@myobie/pty/testing`) — gated by rsync's presence on `$PATH`.

## Layout reference

See [LAYOUT.md](LAYOUT.md). Anything not in LAYOUT.md is implementation
choice and may change.

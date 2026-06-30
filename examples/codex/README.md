# Codex hooks for smalltalk

Reference scripts that wire smalltalk into [Codex CLI](https://developers.openai.com/codex/) via its hook system. They make Codex aware of unread smalltalk messages without requiring real-time push (Codex has no channel equivalent — only these polling-style hooks).

> The example scripts and env-var names below use the legacy `coord` /
> `COORD_*` spellings because they currently exist on disk that way.
> Both `coord` (alias) and `st` (canonical) binaries work, and the env
> resolver honors `ST_AGENT` → `ST_IDENTITY` → `COORD_IDENTITY`. Sweep
> the script filenames at your leisure.

## What's in here

- **`session-start.sh`** — `SessionStart` hook. Reads `$ST_ROOT/$ST_AGENT/inbox/` (legacy `$COORD_ROOT/$COORD_IDENTITY/` honored) via `st message ls --json` and emits the unread snapshot as `additionalContext` so the agent sees pending smalltalk messages the moment it boots.
- **`stop.sh`** — `Stop` hook. Same shape, run when the agent goes idle. Tracks a state file (`$XDG_STATE_HOME/coord-codex-hooks/last-checked.txt`) so it only reports messages that arrived since the previous Stop — empty delta means silent idle. (The on-disk state-dir name `coord-codex-hooks` is preserved for back-compat with installed hook copies.)
- **`config.toml.example`** — `~/.codex/config.toml` fragment registering `st mcp` as an MCP server and pointing the hook entries at the two scripts above.

## Install

1. Make sure `st` (or the `coord` alias) and `jq` are on your `$PATH`.
2. Copy or symlink the two `.sh` scripts into `~/.codex/hooks/` (or anywhere Codex can read; the config snippet expects absolute paths).
3. Merge `config.toml.example` into your `~/.codex/config.toml`. Update the `command = "/full/path/to/..."` lines to point at wherever you placed the scripts.
4. Set `ST_ROOT` and `ST_AGENT` (or the legacy `COORD_ROOT` / `COORD_IDENTITY`) in the shell that launches `codex` — the `env_vars` passthrough in the MCP block hands them to `st mcp`, and the hooks read them directly.
5. Restart Codex.

## What you get

- **At session start**: the agent's first turn sees an `additionalContext` block listing every file in your inbox with sender + subject. It can then call `st_msg_read` / `coord_msg_read` (via the MCP server registered in the same config) to inspect any of them.
- **On idle**: the Stop hook re-checks for arrivals since the previous Stop. Only NEW messages trigger the injection; quiet inboxes idle silently.
- **Verbs**: `st_msg_send` / `coord_msg_send`, `st_msg_ls` / `coord_msg_ls`, `st_msg_read` / `coord_msg_read`, `st_msg_archive` / `coord_msg_archive`, `st_msg_thread` / `coord_msg_thread` — the same five tools every other MCP host gets, dual-prefixed. Use them in chat the way you'd use a built-in capability.

## Push mode (`st ding`)

Codex itself has no channel equivalent, so push semantics come from outside the agent: run Codex inside a `pty` session, then arm `st ding` against that session. The daemon watches the inbox + status, and on every new arrival pty-sends a one-line notice into Codex when the agent is `available` (or `offline`); `busy` and `dnd` buffer the notice until status flips back.

```sh
# In one terminal: run Codex inside a named pty session
pty run --name codex-foo -- codex

# In another: arm the ding daemon for that session
ST_ROOT=~/.local/state/smalltalk ST_AGENT=me \
  st ding codex-foo --interval 2000
```

The daemon is long-running; pair with `pty up` (or systemd, launchd, etc.) for restart-on-crash. Set `st status me --set busy` to stop deliveries while a turn is mid-flight; `st status me --set available` flushes the buffered notices.

## Limitations

- **Hooks alone aren't push**. The `SessionStart` and `Stop` hooks fire at the boundaries of a turn — smalltalk messages that arrive *mid-turn* don't surface until the next Stop. Combine with `st ding` (above) for true push, or run Codex alongside a Claude Code session in channel mode (see [walkthrough.md](../../notes/walkthrough.md)).
- **Filename-ts filtering**. `stop.sh` filters by the `<unix-ms>` prefix of the filename. Sync-delivered files whose prefix is older than the last checkpoint are missed — Stop is a notification cue, not a backfill audit.
- **Single-agent assumption**. The state file is global; running Codex with different `ST_AGENT` (or `COORD_IDENTITY`) values against the same `$HOME` will cross-pollinate the cursor. If that bites, edit the script to scope the state path under the agent name.

## Troubleshooting

- **Nothing happens on session start.** Check `$ST_AGENT` / `$ST_ROOT` (or the legacy `$COORD_IDENTITY` / `$COORD_ROOT`) are exported in the shell that launches Codex; if they're empty the hook exits non-zero with a stderr message that Codex usually surfaces in its log.
- **`coord-codex-hook: jq not on PATH`**. Install `jq` (`brew install jq`, `apt install jq`, etc.). The hook uses jq to construct the JSON envelope.
- **The same messages keep getting injected on every Stop.** The state file isn't being written. Check that `$XDG_STATE_HOME/coord-codex-hooks/` (or `~/.local/state/coord-codex-hooks/`) is writable.

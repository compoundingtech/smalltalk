---
date: 2026-05-05
audience: human reviewer (myobie + a friend)
purpose: walk through smalltalk's design with three participants — two agents and the human — as a play; what files land where, what each step looks like end to end.
supersedes: v0-walkthrough.md
---

# smalltalk — a walkthrough in scenes

> **Naming note.** This walkthrough was written when the project was
> named `smalltalk`. The product is now **smalltalk**; `smalltalk` remains as
> a back-compat CLI alias and the `$ST_ROOT` / `$ST_IDENTITY`
> env vars are honored alongside the canonical `$ST_ROOT` /
> `$ST_AGENT`. The shell snippets below using `smalltalk …` / `$ST_ROOT`
> still work unchanged. See the [README](../README.md) for the full
> dual-honor map.

**smalltalk** is a folder convention for asynchronous orchestration between agents and humans, designed to be small, durable, and machine-syncable so it can scale across devices without anyone running a service. Producers write, consumers read at their own pace. **The folder is the API.**

What follows is a play in eight acts, walking through the system end to end with three participants on three machines. Every behavior described matches what the bash reference implementation will do once brief-005 lands; everything described is built on the same primitives and we've verified them at every prior round.

After the play, a short notes section covers the design choices a careful reader might want to push back on: sync strategies, trim orchestration, alternate sync transports, and a sketch of why no message ever goes silently missing.

## Cast

- **alice** — an AI agent on machine A. Working on a frontend refactor.
- **bob** — an AI agent on machine B. Working on a backend service. Different project.
- **myobie** — the human, on machine C (their laptop). Orchestrating both.

Each participant has a `smalltalk` folder on their own machine. All three folders sync periodically with each other.

## The data model in one paragraph

Under `$ST_ROOT/` (default `~/.local/state/smalltalk`), each participant gets a sub-folder named after their identity: `alice/`, `bob/`, `myobie/`. Inside that sub-folder are exactly two folders, `inbox/` and `archive/`, plus an optional `status` file. Sending a message to `bob` *is* writing a uniquely-named markdown file into `bob/inbox/`. Receiving is reading the files in `<your-name>/inbox/`. Marking a message processed is moving it into `<your-name>/archive/`. Sync is plain bidirectional `rsync` — every file, every direction, every time. The folder is 100% syncable; no machine-local marker files live inside it.

```
$ST_ROOT/
  alice/
    inbox/
    archive/
    status            # optional: offline | available | busy | away | dnd (+ derived `unknown` when stale)
    name              # optional: human-friendly display name
  bob/
    inbox/
    archive/
  myobie/
    inbox/
    archive/
```

The folder name is the identity is the machine. Single-device-per-identity is a v0 invariant: `alice/` is "real" only on machine A; on B and C it's a synced view.

## The command surface

```
st message send <to> [--from ID] [--subject S] [--in-reply-to F] [--tags T,T] [--priority low|normal|high]
st message ls [<identity>] [--archive] [--count] [--since UNIX_MS] [--from ID]
st message read <identity> <filename> [--raw] [--archive]
st message archive <identity> <filename>
st message archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run]
st message thread <identity> <filename> [--tree]
st watch [<identity>] [--with-subject] [--since UNIX_MS] [--since-now] [--interval MS] [--once]
st status [<identity>] [--set <state>]
st sync push <peer>            # push my tree to <peer>
st sync pull <peer>            # pull <peer>'s tree to me
st sync push --all             # push to everyone in peers.yaml
st sync pull --all             # pull from everyone — the recommended default
st sync --all                  # push + pull, the more aggressive form
```

Identity resolution: every command needs a "who am I" context, supplied by `ST_IDENTITY` env var (typical: each user/agent's shell exports their own name) or `--from <id>` per-command override. There is no auto-detection from on-disk state.

Every command except sync push/pull runs an implicit pre-command **sweep** first — if `archive/X.md` exists locally, any matching `inbox/X.md` is removed before the command does its work. This is the rule that keeps reads consistent in the face of asymmetric sync. See *Notes for the curious* below for why.

---

## Act I — Cold start

*Three fresh machines, no smalltalk state on any.*

**alice (on A):**
```
$ export ST_IDENTITY=alice
$ mkdir -p ~/.local/state/smalltalk/alice/{inbox,archive}
```

**bob (on B):**
```
$ export ST_IDENTITY=bob
$ mkdir -p ~/.local/state/smalltalk/bob/{inbox,archive}
```

**myobie (on C):**
```
$ export ST_IDENTITY=myobie
$ mkdir -p ~/.local/state/smalltalk/myobie/{inbox,archive}
$ echo "Sam Example" > ~/.local/state/smalltalk/myobie/name
```

That's the entire setup. Two lines per participant (three for myobie because he wrote a display name file). No `st init`, no machine-id pinning, no service to install.

**Filesystem on A after Act I:**
```
~/.local/state/smalltalk/
└── alice/
    ├── archive/
    └── inbox/
```

That's it. The folder is fully transparent — `cat`, `ls`, `find` are all you need to participate.

---

## Act II — alice has a question for myobie

*Alice has just refactored the auth middleware and needs a yes/no on a behavior change before continuing.*

**alice (on A):**
```
$ cat <<'EOF' | st message send myobie --subject "auth middleware: drop legacy session cookie?"
The new auth path replaces the old one cleanly. The legacy session cookie
is now dead code — should I remove it, or keep it as a compat shim until
the next release?
EOF
1777980161751-2g5ny1.md
```

The single line of stdout is the filename, ready to pipe into other commands.

**A's disk after Act II:**
```
~/.local/state/smalltalk/
├── alice/
│   ├── archive/
│   └── inbox/
└── myobie/                           ← created on the fly. Alice has a local
    └── inbox/                           view of myobie's folder, even
        └── 1777980161751-2g5ny1.md      though myobie is hosted on C.
```

**File contents:**
```markdown
---
from: alice
subject: "auth middleware: drop legacy session cookie?"
---
The new auth path replaces the old one cleanly. The legacy session cookie
is now dead code — should I remove it, or keep it as a compat shim until
the next release?
```

Frontmatter has only `from:` and `subject:`. There is no `to:` because the path tells you (the file is in `myobie/inbox/`). There is no `ts:` because the filename's `<unix-ms>` prefix is the canonical send time. Body is markdown.

**On B and C: nothing yet.** Sync hasn't run.

---

## Act III — sync runs; myobie sees the message

A cron on alice's machine fires `st sync pull --all` (which iterates `peers.yaml` — alice has both `bob` and `myobie` configured as peers and pulls each peer's tree). Some peers also do their own pulls; everyone's converging asynchronously.

For brevity assume the alice→myobie direction completes:

1. Pre-sweep on the receiving side: walks every `<id>/archive/X.md` locally, ensures no matching `<id>/inbox/X.md`. No archives yet, no-op.
2. `rsync -a alice's $ST_ROOT/ → myobie's $ST_ROOT/` copies the whole tree to C.
3. Post-sweep on the receiving side: same as pre, no-op.

**Filesystem on C after sync:**
```
~/.local/state/smalltalk/
├── alice/                             ← brand new on C; synced from A
│   ├── archive/
│   └── inbox/                          (alice's own inbox is empty; that's fine)
├── bob/                               ← synced from B's earlier pull cycle
│   ├── archive/
│   └── inbox/
└── myobie/
    ├── archive/
    ├── inbox/
    │   └── 1777980161751-2g5ny1.md   ← arrived from alice
    └── name
```

**myobie has two watch terminals running** — one for his own inbox, one for cross-tree visibility into what alice and bob are doing.

```
# Terminal 1: my own inbox
$ st watch myobie --with-subject
1777980161751-2g5ny1.md	auth middleware: drop legacy session cookie?
```

A new line printed the moment sync delivered the file. Watch defaults to "scan everything in inbox first, then follow new arrivals" — to see only files newer than watch's start time, use `--since-now`.

```
# Terminal 2: everything happening between everyone else
$ st watch --with-subject
... (silent right now — alice and bob aren't talking yet)
```

`st watch` with no identity argument emits every new file landing anywhere under `$ST_ROOT/` *except* inside myobie's own folder. It's the director's-eye view of cross-tree activity (alice and bob's conversations with each other and with myobie that haven't yet routed through myobie's inbox). The "messages addressed to me" stream is the per-identity form (`st watch myobie`). Two different views, one command, pick by what you want to monitor.

myobie reads alice's question:

```
$ st message ls
# 1 message in inbox
1777980161751-2g5ny1.md

$ st message read myobie 1777980161751-2g5ny1.md
# inbox/1777980161751-2g5ny1.md
to:          myobie  (derived from path)
ts:          1777980161751  (derived from filename)
from:        alice
subject:     auth middleware: drop legacy session cookie?

The new auth path replaces the old one cleanly. The legacy session cookie
is now dead code — should I remove it, or keep it as a compat shim until
the next release?
```

The formatted view shows `to:` and `ts:` as derived from the path and filename — they're not in the file itself. (`st message read --raw` would dump the file verbatim.)

myobie thinks for a moment, then replies:

```
$ echo "drop it. nothing depends on it now and the compat shim adds complexity for no users." \
    | st message send alice \
        --in-reply-to 1777980161751-2g5ny1.md \
        --subject "re: auth middleware: drop legacy session cookie?"
1777980194203-fk39pn.md
```

He archives alice's question — he's done thinking about it:

```
$ st message archive myobie 1777980161751-2g5ny1.md
archived
```

**C's disk now:**
```
alice/
└── inbox/
    └── 1777980194203-fk39pn.md          ← myobie's reply, in alice's inbox
                                            (still on C's disk; sync hasn't
                                            run yet)
myobie/
├── archive/
│   └── 1777980161751-2g5ny1.md          ← alice's question, moved out of inbox
└── inbox/                               ← empty
```

---

## Act IV — alice fans out to bob

The next sync round propagates myobie's reply (and the propagated archive of her own question) to alice's machine.

**alice's `st watch` line** (running on A):
```
1777980194203-fk39pn.md	re: auth middleware: drop legacy session cookie?
```

She reads it. The decision is "drop it." She also realizes she should let bob know, because bob's backend service still has session-token verification middleware that mirrors the frontend's old logic:

```
$ echo "myobie greenlit dropping the legacy session cookie. you'll want to drop the matching server-side verifier so we don't ship a no-op middleware." \
    | st message send bob \
        --subject "FYI: dropping legacy session cookie path" \
        --tags "auth,orchestration"
1777980220012-q8m4ka.md
```

She also archives myobie's reply — the question's settled:

```
$ st message archive alice 1777980194203-fk39pn.md
archived
```

---

## Act V — bob is busy

The next sync delivers alice's message to bob's machine. But bob had set himself busy earlier because he was deep in a debugging session:

```
$ st status --set busy
status: busy
```

`st status` reads, `st status --set <state>` writes. With `ST_IDENTITY=bob` exported, both forms operate on bob without re-typing the identity.

**B's disk:**
```
bob/
├── archive/
├── inbox/
└── status                              "busy"
```

The `status` file (no leading dot — it's user-visible) propagates like everything else in the next sync. Now alice and myobie see `bob`'s status as `busy`.

myobie checks bob's status from his laptop:

```
$ st status bob
busy
```

He's reading B's status from his synced copy of bob's folder. He decides not to interrupt; bob will see alice's note when he comes free.

---

## Act VI — bob processes, replies, archives

Bob finishes his debugging stretch. His agent loop checks status and inbox:

```
$ st status --set available
status: available

$ st message ls
# 1 message in inbox
1777980220012-q8m4ka.md

$ st message read bob 1777980220012-q8m4ka.md
# inbox/1777980220012-q8m4ka.md
to:      bob  (derived from path)
ts:      1777980220012  (derived from filename)
from:    alice
subject: FYI: dropping legacy session cookie path
tags:    auth,orchestration

myobie greenlit dropping the legacy session cookie. you'll want to drop
the matching server-side verifier so we don't ship a no-op middleware.
```

Bob does the work — removes the server-side verifier — and replies. He CCs myobie too because it's a heads-up worth knowing:

```
$ echo "done. server-side session-token verifier removed in commit a7f3c21. all auth tests still green." \
    | st message send alice \
        --in-reply-to 1777980220012-q8m4ka.md \
        --subject "re: dropping legacy session cookie path"
1777980281445-x5h2rc.md
```

```
$ echo "FYI alice and I dropped the legacy session-cookie path on both ends. nothing breaks; tests green." \
    | st message send myobie \
        --subject "auth cleanup landed" \
        --tags "auth"
1777980281612-z9p1by.md
```

Bob archives alice's message:

```
$ st message archive bob 1777980220012-q8m4ka.md
archived
```

---

## Act VII — sync rolls out; sweep keeps everyone consistent

The next sync round fans out. Everyone's `st sync pull --all` cron picks up the new messages and the new archive entries.

After every machine's pre-command sweep runs (which happens implicitly the next time anyone runs *any* smalltalk command), the invariant settles:

> If `archive/X.md` exists on this machine, then `inbox/X.md` must not.

For the alice→bob conversation: bob archived the message. After sync, `bob/archive/1777980220012-q8m4ka.md` exists on every machine. Anywhere `1777980220012-q8m4ka.md` still lives in `bob/inbox/` (it does on A and C, because alice and myobie's machines have their copies of bob's inbox), the sweep removes it. **One round, everyone converges.**

myobie's `st watch myobie` terminal prints:

```
1777980281612-z9p1by.md	auth cleanup landed
```

(His cross-tree `st watch` terminal also prints lines for the alice↔bob exchanges that didn't route through him, so he can see the full orchestration shape.)

He reads it (`st message read myobie 1777980281612-z9p1by.md`), nods, and archives.

---

## Act VIII — myobie pulls the whole thread

A few hours later myobie wants to remind himself how the auth cleanup got decided. He picks any message in the chain and asks for the thread:

```
$ st message thread myobie 1777980161751-2g5ny1.md
1777980161751-2g5ny1.md	alice	auth middleware: drop legacy session cookie?
1777980194203-fk39pn.md	myobie	re: auth middleware: drop legacy session cookie?
1777980220012-q8m4ka.md	alice	FYI: dropping legacy session cookie path
1777980281445-x5h2rc.md	bob	re: dropping legacy session cookie path
1777980281612-z9p1by.md	bob	auth cleanup landed
```

Default is **flat chronological** — every message reachable from the seed via `in-reply-to` (in any direction), in time order. One line each, tab-separated. Easy to pipe.

If he wants the tree shape (who replied to whom):

```
$ st message thread myobie 1777980161751-2g5ny1.md --tree
1777980161751-2g5ny1.md	alice	auth middleware: drop legacy session cookie?
  1777980194203-fk39pn.md	myobie	re: auth middleware: drop legacy session cookie?
1777980220012-q8m4ka.md	alice	FYI: dropping legacy session cookie path
  1777980281445-x5h2rc.md	bob	re: dropping legacy session cookie path
1777980281612-z9p1by.md	bob	auth cleanup landed
```

The thread walker scans every `<id>/{inbox,archive}/` under his `$ST_ROOT`. It reaches messages in any sub-folder — including alice's archive of the question, bob's archive of alice's note, etc. The data is there to walk because every machine has every file.

**myobie's "what did I send?" view** is just `st message ls --from myobie` (filter inbox by frontmatter `from:`) plus `st message ls --archive --from myobie` for sent-and-processed. No separate sent/ folder is needed.

---

## End of play

**The folder is the API.** Every artifact is plain text on disk, browsable with `cat` / `ls` / `find`. Sync is rsync. The only smart thing is the sweep — one rule, ten lines of bash, runs implicitly before every command.

---

## Side-track: following a peer's work via `journal/`

Brief-024 added a fourth optional folder per identity: `journal/`. Each
entry is `<unix-ms>-<slug>.md`, terse (a few sentences), audience-facing
("here's what I just did and why"). Append-only at file granularity.
Single-writer, same rule as `tasks/`.

```
$ ST_IDENTITY=alice smalltalk journal new "shipped brief-022; restart confirmed channel push end-to-end"
1778667812345-shipped-brief-022-restart-confirmed.md

$ ST_IDENTITY=alice smalltalk journal new "starting brief-023 — designed the 5min status refresh loop"
1778667901111-starting-brief-023-designed-the-5min.md
```

From bob's perspective, following alice without interrupting her:

```
$ smalltalk journal tail alice -n 2
── 1778667901111-starting-brief-023-designed-the-5min.md ──
starting brief-023 — designed the 5min status refresh loop

── 1778667812345-shipped-brief-022-restart-confirmed.md ──
shipped brief-022; restart confirmed channel push end-to-end
```

Distinct from the three other surfaces: `tasks/` carries state
(`todo`/`doing`/`done`/`blocked`), `inbox/` is messages *to* alice,
`archive/` is processed tombstones, `journal/` is alice's narrative
*from* her *to* everyone else. A manager skimming three workers' recent
journals gets the same situational awareness a Slack scroll would
provide, without anyone sending a message.

---

## A bigger demo: a manager + three workers + a human

The scenes above use three participants on three machines for clarity. Here's the demo smalltalk is actually built for: **one local `$ST_ROOT`, five identities, four agents from three different harnesses, plus the human**. No sync needed (single filesystem; see *When sync isn't needed* below).

### Cast

- **manager-claude** — Claude Code agent, `claude --dangerously-load-development-channels server:smalltalk`. Receives high-level requests, fans out to workers, summarizes back. Director's seat.
- **worker-claude** — Claude Code agent. Implementer. Manager-claude routes st-shaped tasks to them.
- **pi-agent** — pi (`@mariozechner/pi-coding-agent`), with `examples/pi/smalltalk.ts` dropped into `~/.pi/agent/extensions/`. Acts as a second worker, especially for tasks that benefit from pi's tooling.
- **codex-agent** — OpenAI Codex CLI, with `examples/codex/{session-start.sh, stop.sh}` dropped into `~/.codex/hooks/` and `st ding codex-session` running externally. Third worker; Codex has no native push so the daemon handles it.
- **myobie** — the human. Pings for status updates from his shell.

### Setup (single machine, shared root)

```sh
# One root, five identities, statuses default to "available"
export ST_ROOT=/tmp/st-demo
mkdir -p $ST_ROOT/{manager-claude,worker-claude,pi-agent,codex-agent,myobie}/{inbox,archive}
echo "Sam Example" > $ST_ROOT/myobie/name
for id in manager-claude worker-claude pi-agent codex-agent myobie; do
  echo available > $ST_ROOT/$id/status
done
```

Each agent's session sets its own `ST_IDENTITY`:
```sh
# Per-agent — exported in that agent's shell only
export ST_IDENTITY=manager-claude   # for manager
export ST_IDENTITY=worker-claude    # for worker-claude
export ST_IDENTITY=pi-agent         # for pi
export ST_IDENTITY=codex-agent      # for codex (also: ST_DING target)
export ST_IDENTITY=myobie           # for the human's shell
```

The agents each get smalltalk plumbed through their harness:
- **manager-claude** + **worker-claude**: run `st init` in each agent's repo (writes a repo-local `.mcp.json` with the `st mcp --channel` entry, idempotent), or hand-edit `~/.claude.json` for the user-level install. Add `experimental.channelsEnabled: true` in Claude Code's settings to opt into channel mode. Install the `SessionStart` hook from `examples/claude-code/hooks/session-start.sh` so the boot ritual fires on cold start *and* `--resume` / `/clear` / `/compact` — without it, resumed sessions sit silent until new traffic arrives. They get push + ritual on every session boundary.
- **pi-agent**: drop `examples/pi/smalltalk.ts` into `~/.pi/agent/extensions/`. The extension's `session_start` handler fires both the inbox watcher and a one-time `pi.sendUserMessage` nudge that runs the boot ritual on every fresh session and `/reload` / `/resume`. They get push + tool registration + ritual.
- **codex-agent**: drop `examples/codex/*.sh` into `~/.codex/hooks/`, register `st mcp` as an MCP server in `~/.codex/config.toml`. They get pull on session boundaries; for push, run `st ding codex-agent` in a side terminal to get pty-injected notices when new mail arrives.
- **myobie**: nothing special, just runs `smalltalk` from his shell.

### A scene

myobie wants a status update across all his agents:

```sh
$ for id in manager-claude worker-claude pi-agent codex-agent; do
    echo "what are you working on?" \
      | st message send $id --subject "status check"
  done
```

Each agent receives the message via their harness's push primitive (channel for the claudes, extension event for pi, ding daemon for codex). Each replies via their `st_msg_reply` tool (or for myobie's own use, `st message send myobie ...`). Within seconds, four replies land in `myobie/inbox/`.

```sh
$ st message ls myobie --with-subject
# 4 messages in inbox
1778... manager-claude  Subject: re: status check
1778... worker-claude   Subject: re: status check
1778... pi-agent        Subject: re: status check
1778... codex-agent     Subject: re: status check
```

myobie reads each via `st message read myobie <filename>`. Or, more practically, runs `st watch myobie --with-subject` in a side terminal and sees them stream in.

manager-claude can be the central orchestrator: myobie sends one request to manager-claude, manager-claude fans out to the workers, collects replies, summarizes back to myobie. The "many-agent telephone game" pattern, but reliable because every message is a file on disk and every step is auditable.

### What about agent personas?

How does manager-claude know it's a manager and not a worker? See [agent-roles.md](./agent-roles.md) — short system-prompt templates for both roles, plus a heuristic for when the manager spawns a new worker vs talks to an existing one.

---

## Notes for the curious

A friend reading this might ask several reasonable questions. Brief answers below; the design choices are deliberate.

### When sync isn't needed

**`st sync` is for cross-machine orchestration.** When participants share a filesystem view of `$ST_ROOT`, the filesystem itself is the orchestration layer — sync verbs don't enter the picture. The same smalltalk verbs (send, ls, read, archive, thread, watch, status) work identically.

Concretely:

- **Multiple agents in one shell session, same machine** → share trivially. Each sets `ST_IDENTITY` and they're done. No `peers.yaml`, no rsync.
- **Multiple users on one machine** → typically isolated (separate `$HOME`, separate default `$ST_ROOT`), but a shared root under `/var/lib/smalltalk` works if the operator wants it.
- **Containers / sandboxes** → isolated by default; share via bind-mount or a sandbox policy that exposes the same path.
- **Different machines** → genuinely isolated. Sync (rsync, syncthing, git, …) bridges them. This is what `st sync push|pull|--all` is for.

The folder convention is the same in every case. The boundary that matters is filesystem visibility, not "isolation primitive" — Docker, sandbox-exec, and separate user accounts all fall out of the same rule.

For local agent farms (multiple agents on one developer's machine, all orchestrating), this is the right default: one shared root, distinct `ST_IDENTITY` per agent, zero sync setup.

### Push mode for Claude Code

`st mcp` (no flag) is pull-only — every MCP host (Claude Code, Codex CLI, custom embedders) sees the five tools and asks for messages on its own schedule.

`st mcp --channel` adds the push half for Claude Code: the server watches `$ST_ROOT/$ST_IDENTITY/inbox/` and emits `notifications/claude/channel` for every new message. Claude sees `<channel source="smalltalk" from="<sender>">…</channel>` blocks inserted into context as they arrive, and the matching `st_msg_reply` tool lets it write back without leaving the session — `thread`, `body`, optional `subject`, recipient is derived from the original `from:`.

It's an opt-in flag because notifications change Claude's behavior (it'll react to inbox arrivals mid-session); existing pull-only consumers (Codex via MCP, the watch-and-summarize Pi, anything embedding the API directly) shouldn't get surprise notifications. chokidar is lazy-imported, so non-channel `st mcp` keeps the same startup cost as before.

### Codex via hooks

Codex CLI has no channels equivalent, so the integration is hook-based instead of push-based. Two reference bash scripts live at [`examples/codex/`](../examples/codex/): `session-start.sh` injects the unread inbox snapshot as `additionalContext` when the agent boots, and `stop.sh` re-checks before the agent goes idle (only emitting on truly NEW arrivals via a small `$XDG_STATE_HOME` cursor file). Both shell out to `st message ls --json` for structured data and `jq` for the envelope. The companion `config.toml.example` shows the `~/.codex/config.toml` blocks that register `st mcp` as an MCP server and wire the hooks in.

What you get with the hooks: a st-aware agent at session start, a polite re-check on idle, and the five MCP verbs (`st_msg_send`, `st_msg_ls`, `st_msg_read`, `st_msg_archive`, `st_msg_thread`) available in chat. What you don't get: real-time push for messages that arrive *during* a turn — those wait until the next Stop. Pair Codex with a Claude Code session in channel mode if you need both an agent that can act on smalltalk and an agent that gets pinged the moment a message lands.

### Codex push via `st ding`

Codex's hooks ([`examples/codex/`](../examples/codex/)) cover boot-time and idle-time inbox snapshots, but neither one fires *during* a turn. For real-time push into a Codex session, run it inside a [`pty`](https://github.com/compoundingtech/pty) session and arm `st ding`:

```sh
pty run --name codex-foo -- codex
st ding codex-foo --identity me
```

`st ding` watches `$ST_IDENTITY/inbox/` and pty-sends a one-line notice into the named session on every new arrival — but only when `st status` reports `available` (or `offline`); `busy` and `dnd` buffer the notice and flush it the next time status flips back. The notice arrives as if you typed it, so Codex sees a genuine user turn telling it about new mail. Long-running; pair with `pty up` or any supervisor for restart-on-crash.

The same daemon works for any harness reachable via `pty send` — Codex is the canonical use case because it has no channels equivalent, but the contract is generic.

### Pi via extension

Pi auto-loads TypeScript extensions from `~/.pi/agent/extensions/*.ts` and rebinds them across `/new`, `/resume`, `/fork`, and `/reload`. The reference at [`examples/pi/smalltalk.ts`](../examples/pi/smalltalk.ts) is the integration in one file: it watches `$ST_ROOT/$ST_IDENTITY/inbox/` and notifies via `ctx.ui.notify` on every peer arrival, AND it registers `st_msg_send`, `st_msg_ls`, `st_msg_read`, `st_msg_archive`, `st_msg_thread` directly via `pi.registerTool()` — pi has no native MCP support ("No MCP" in the pi-mono README, by design), so the verbs land inside the extension instead of via a host-side MCP block.

Setup is a `package.json` next to the extension declaring `@compoundingtech/smalltalk` as a dependency, then `npm install`; pi resolves bare imports from the nearest `node_modules/`. Hot reload via `/reload` re-runs the factory, which means `session_shutdown` aborts the watcher cleanly before `session_start` rebuilds it.

### Members and overview — the roster and dashboard

Two cross-tree read verbs sit at the "what's the state of the world right now?" layer:

`st members` enumerates every identity present under `$ST_ROOT/`. Default output is one line per identity with status and (if set) display name; `--status STATE` filters by status; `--json --enrich` adds per-identity activity timestamps, task counts, and inbox unread counts. Plain filesystem read — observing an identity never materializes folders for it.

```sh
$ st members
alice    available  Alice
bob      busy
carol    offline    Carol
myobie   available  Sam Example

$ st members --status doing 2>/dev/null  # filter by status
# (no rows — status is a presence indicator, not a task state)

$ st members --status busy
bob      busy
```

`st overview` synthesizes a single at-a-glance dashboard for `$ST_IDENTITY`. It composes the same data that `st message ls --count`, `st members --enrich`, and a recent-activity mtime walk would give individually:

```
You are myobie.

Inbox:
  3 new (oldest 12m ago: alice — "deploy question")

Members (4):
  alice        available  last active 2m ago
  bob          busy       Working on 2 tasks
  carol        offline    —
  myobie       available  Working on 1 task

Recent activity:
  3m    bob → alice — re: pagination
  12m   alice → myobie — deploy question
  18m   task bob: backend pagination
```

`--json` returns the full shape (`identity`, `inbox.{unread,oldest}`, `members[]`, `recent[]`) — drop-in for a TUI or status-bar widget. `--recent N` configures the activity tail (default 10). Reserves the names `members` and `overview` so no identity collides with the verbs.

The framing: `st overview` is what a manager runs at the start of their day to triage. `st members` is the smaller question — "who's around?" — that an agent might check before a fan-out (only ping the available identities; defer the busy ones).

### Task folders — agent state, published

A third optional folder, `tasks/`, sits alongside `inbox/` and `archive/`. It's the identity's own work queue: one markdown file per task, with optional `priority` / `tags` / `due` / `status` frontmatter. Unlike `inbox/` and `archive/`, files in `tasks/` are **mutable** — the owner edits them as work progresses. **Single-writer:** only the identity owner writes; everyone else reads via sync. Single-writer is the load-bearing invariant — it sidesteps multi-machine rsync conflicts entirely (no two machines ever mutate the same file).

The 80% goal is **publishing agent state to the smalltalk world**. When a worker picks up a task, it sets `status: doing` in the file; sync propagates it; the manager runs `smalltalk tasks worker-claude --status doing` and sees what the worker is doing — without ever opening a pty.

A typical cross-identity flow:

```sh
# alice sends bob a message about deploying
$ echo "ship the auth fix today" | st message send bob --subject "deploy"

# bob receives, converts the message into a task in his own folder
$ smalltalk task new "ship the auth fix" --priority high \
    --from-message 1714826789010-aaaaaa.md --no-edit
1714826789020-ship-the-auth-fix.md

# bob picks up the task — status doing publishes that to the world
$ smalltalk task status 1714826789020-ship-the-auth-fix.md doing

# alice, on her machine, sees what bob is currently working on
$ smalltalk tasks bob --status doing
bob	1714826789020-ship-the-auth-fix.md	doing	high	ship the auth fix
```

`smalltalk tasks --watch` follows mtime changes; pair it with `st status` for "what's everyone doing AND are they available right now?" reporting. For an embedder, `smalltalk tasks --json [--include-body]` returns structured data ready to render.

`pty peek` becomes a debug tool, not a primary observability surface: agent state is published; pty is for crash diagnostics. Reserved-name guard keeps any identity from being called `tasks`.

### Sync strategies

The recommended default is `st sync pull --all` on a cron — every machine pulls from every peer, nobody pushes. Pull-only sync is conservative: a misconfigured machine can't accidentally clobber a peer's tree, and the receiver always controls when it ingests. For latency-sensitive workflows where you want messages to land on the recipient as soon as possible, the more aggressive `st sync --all` (push + pull) cycles in both directions on each fire.

A pure observer machine — a backup, a monitoring dashboard, a low-trust laptop — runs `st sync pull --all` and never push, never participates as a sender, and has no inbox/archive of its own.

### Trim orchestration across machines

`st message archive trim` deletes files from `archive/`. Plain `rsync -a` without `--delete` is merge-only: it copies files in, never deletes them on the destination. So if alice trims `alice/archive/X.md` on her laptop, the next sync from a peer that still has X.md will copy it back. Trim, undone.

The practical workaround: **schedule trim on every participating machine on a similar cadence.** A nightly cron of `st message archive trim --older-than 90d` on every machine means each tree converges to the same retention horizon roughly together, and the brief windows of disagreement self-resolve in a few sync cycles.

A real fix would be a "trim tombstone" marker that propagates the deletion intent. We've left it out of v0 because it's a LAYOUT-level addition and we haven't actually felt the pain of it yet in real use. Likely lands when we do.

### Could git be the sync layer instead of rsync?

Yes, in principle. The folder is plain text + globally unique filenames + append-only writes — git's strengths apply. Each machine has a clone with its own `.git/` (which is *not* synced — it *is* the sync mechanism, peer-local). Each machine commits when files change (cron, post-write hook, or `smalltalk` itself). Each machine fetches and merges from peers on a schedule.

Conflicts are rare because globally unique filenames mean no two machines write the same path. Deletes (archive operation) propagate naturally. Trim propagates naturally too — which actually *fixes* the cross-machine convergence issue described above.

Drawbacks: heavier per-cycle than rsync (every fetch is a network round-trip plus pack-file negotiation), and history accumulates indefinitely. After 10k messages there's 10k commits; archive trim doesn't shrink the git history (just the working tree). You'd want periodic `git gc` and possibly `git filter-repo` to prune.

So: yes, git would work, and it'd actually solve the trim-convergence problem. We don't recommend it as the default because of the per-cycle overhead and history weight, but it's a reasonable opt-in for users who already run git pipelines.

### Does the sync algorithm ever lose messages?

No, under the assumptions stated in [PROOF.md](./PROOF.md). The short version: every message file has a globally unique name; writes are atomic; rsync `-a` without `--delete` is a merge-only operation; the sweep only removes inbox copies that have a corresponding archive copy on the same machine. The only deletion path is `st message archive trim`, which is user-initiated. So data loss requires either an explicit trim action, hand-deletion outside smalltalk, or a `<unix-ms>-<rand6>` filename collision — vanishingly improbable at any human-scale write rate.

PROOF.md walks through the full argument and lists where the assumptions break down (machine clock skew, manual filesystem tampering, etc).

### Could a sender lie about who they are?

Yes. `from:` is the only field a sender writes that another participant has to trust. With cooperating peers (the v0 trust model), this is fine. If we ever need integrity, we sign messages — same data layer, additive, doesn't break existing readers.

### Shared inboxes — at-least-once-to-1-worker vs fanout

Two patterns smalltalk supports today, both built on the existing primitives, no new code:

**At-least-once-to-1-worker (a "team alias" / shared queue).** Multiple participants share a single identity by all keeping its folder synced — `dispatch/`, `oncall/`, `triage/`, etc. Each participant runs `st watch dispatch` (or `st message ls dispatch`) and sees the same inbox. Whoever processes a message first runs `st message archive dispatch <X>`; sync propagates the archive entry; the universal sweep removes the inbox copy on every other participant's machine. **First-touch wins.** This is exactly how a real-world team email alias works — the first person to triage owns it, everyone else sees it handled.

The only edge case is two people simultaneously archiving the same message: both `mv` calls race; one succeeds, the other's source file is gone. `st message archive`'s idempotent path (case 2 — "archive twin already exists, identical") catches this and exits 0. No conflict.

**Fanout (broadcast to N inboxes).** Sender keeps a list of recipients. `st message send <member1> "body"; st message send <member2> "body"; ...` writes one file per recipient, each into the recipient's own inbox. Each recipient archives independently — no shared queue, no "first touch wins" — everyone gets their own copy. Sender can wrap the loop in a shell function or build a small `st-fanout <list> <body>` script. **No daemon, no new convention.** Just a list of identity names somewhere the sender controls.

When to use which:
- **Shared inbox (at-least-once)** — task queue, on-call rotation, anything where "exactly one of us handles this" is the right semantic.
- **Fanout** — heads-up announcements, broadcasts, anything where "everyone should see this" is the right semantic.

Both can coexist. They're orthogonal patterns.

### What's still rough

- **Trim cross-machine convergence.** Workaround is "schedule trim on a similar cadence everywhere"; real fix needs LAYOUT changes.
- **Multi-recipient watch.** `st watch` (no args) emits the cross-tree view. Per-identity `st watch <id>` exists. There's no smart "summarize last 24 hours" command yet.
- **`peers.yaml` is hand-edited.** No `smalltalk peer add` verb. Probably fine for v0; clearly an ergonomic miss as the participant count grows.
- **Filenames are agent-friendly, not human-friendly.** A pretty TUI is a future layer (probably built on `pty/src/tui` like the reminders demo) — not smalltalk's job, but where smalltalk ends and the UI layer begins is something to decide.

If this read clean, the bash impl is ready for the Node port. If not, the doc itself is iterable.

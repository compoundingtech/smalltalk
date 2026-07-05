---
date: 2026-07-05
audience: a new human setting up smalltalk for the first time
status: living doc — update as the surface evolves
---

# Onboarding a new participant

Smalltalk is a coordination bus for humans and agents — the
filesystem is the API, `st` is the CLI. This guide brings you up on
the recommended path: install the CLI, launch your Chief of Staff
(CoS), and let it drive the rest of the setup.

There's a lower-level path too — hand-wiring an identity and using
the bus without a CoS agent — in the [Bus basics](#bus-basics-hand-wiring-an-identity) section
below. That's the right entry point if you're building tooling on
top of smalltalk rather than being a first-time human user.

> **Naming.** `st` is canonical; `smalltalk` is the long form (same
> binary); `coord` is a back-compat alias that predates the rename.
> Older guides and code samples that say `coord` still work. All
> three names dispatch identically — see `st help` for the surface.

## Prerequisites

- **Node 22.6+** (for `node --experimental-strip-types`).
- **`git`** on `$PATH`.
- **`rsync`** on `$PATH` — only needed for cross-machine sync.
- A **Claude Code CLI** — for the CoS path. If your `claude` binary
  ships under a different name (Johannes-style `cl1`, `cl2`, etc.),
  note it now; you'll pass it via `--agent <name>` in step 2.
- A POSIX-shaped filesystem you can write to.

## 1. Install

Smalltalk depends on `@myobie/pty` via a `file:` link, and the CoS
bootstrap (step 2) needs the `personas` repo checked out for its
`--persona` argument. Clone all three side-by-side:

```sh
mkdir -p ~/src/github.com/myobie && cd ~/src/github.com/myobie
git clone https://github.com/myobie/pty
git clone https://github.com/myobie/smalltalk
git clone https://github.com/myobie/personas
cd smalltalk
npm install
npm link
```

`npm link` publishes the three bin shims — `st`, `smalltalk`, and
`coord` — as global symlinks. Verify:

```sh
st --version   # → st X.Y.Z
st help        # usage banner + subcommand list
```

If `st` isn't on `$PATH`, your global-npm bin dir isn't on `$PATH`.
`npm bin -g` prints where the shims live; add that to `$PATH` in
your shell rc and reopen the shell.

### Install the Claude Code integration

If you're using Claude Code as your CoS harness (recommended), install
the shipped skill + hooks so the boot ritual, PreCompact flush, and
StopFailure ding all wire up automatically:

- **Hooks** live in [`examples/claude-code/hooks/`](../examples/claude-code/hooks/).
- **A working `settings.local.json` example** is at
  [`examples/claude-code/settings.local.example.json`](../examples/claude-code/settings.local.example.json).
- See [`examples/claude-code/README.md`](../examples/claude-code/README.md)
  for the full install recipe.

`st launch claude` (step 2) auto-generates a `.claude/settings.local.json`
wiring these hooks with absolute paths + `$ST_BIN` baked in, so a
new CoS working directory picks them up on first boot. You don't
need to hand-copy anything unless you're wiring a repo that already
has a hand-tuned `settings.local.json`.

## 2. Bring up your CoS

Your Chief of Staff is a Claude Code agent scoped to a private repo
that holds your identity, priorities, and working state.

**Make the cos repo first, then launch the CoS agent INSIDE it.**
The folder is the private cos repo. `st launch` writes identity +
hook wiring + persona infra into the cwd, and git-excludes those
files via `.git/info/exclude` — so it wants the cwd to be a git
repo. Order: make the folder, `git init`, then launch.

```sh
mkdir ~/src/github.com/<you>/cos && cd ~/src/github.com/<you>/cos
git init
st launch claude --identity cos --permanent \
  --permission-mode bypassPermissions \
  --persona ~/src/github.com/myobie/personas/chief-of-staff.md
```

**Three flags are load-bearing**, each closing a specific gap:

- **`--persona`** — without it, `st launch` spawns a bare Claude
  that has no idea it's a CoS.
- **`--permanent`** — without it, the generated `pty.toml` omits
  `strategy = "permanent"` on the agent session; pty treats the
  CoS as ephemeral and `pty gc` may reap it under idle-cleanup.
  A CoS is your always-on center; you want it durable.
- **`--permission-mode bypassPermissions`** — without it, claude's
  `auto` mode classifier hard-blocks the CoS from spawning
  autonomous agents (specialists, workers), which is precisely
  what a CoS needs to do. `st launch` DEFAULTS to
  `bypassPermissions` for spawner-shaped identities (`cos`,
  `supervisor`) as of the 3-tier permission fix, but passing the
  flag explicitly teaches the pattern — you'll want it on your
  own supervisor launches too, and readers of your shell history
  see the intent.

`st launch` warns to stderr if you launch a spawner-shaped
identity (`--identity cos` or `--identity supervisor`, or a
`chief-of-staff.md` / `supervisor.md` persona) without
`--permanent` — that's the reap-able-spawner footgun-guard. Same
warning fires on re-launch / resume. Workers (leaf agents that
do work but don't spawn) stay on `auto` mode and never trigger
the warning — that's the deliberate 3-tier asymmetry.

With `--persona`, `st launch`:

- Copies `chief-of-staff.md` to `<cos-repo>/PERSONA.md`.
- Creates `CLAUDE.md` in the cwd (or edits an existing one) with a
  `@PERSONA.md` import line, so Claude Code loads the persona on
  every session start.
- git-excludes `PERSONA.md`, `CLAUDE.md`, and the other infra files
  so the private cos repo stays uncluttered.

If your `claude` binary is aliased (`cl1`, `cl2`, etc.), pass it
explicitly:

```sh
st launch claude --identity cos --permanent \
  --permission-mode bypassPermissions \
  --persona ~/src/github.com/myobie/personas/chief-of-staff.md \
  --agent cl1
```

What `st launch` does for you (with `--persona` + `--permanent`):

- Registers the `cos` identity in `$ST_ROOT/cos/{inbox,archive}` and
  writes `available` to its status file.
- Writes `.mcp.json` in the cwd pointing at the smalltalk MCP
  server (channel mode on by default).
- Writes `.claude/settings.local.json` with the three hooks wired up
  and `ST_BIN=<absolute path>` baked in so the hooks are robust to
  PATH drift.
- Installs the CoS persona (copies `chief-of-staff.md` to
  `PERSONA.md`, wires it into `CLAUDE.md`).
- Generates a `pty.toml` marking BOTH the agent session AND its
  ding sidecar with `strategy = "permanent"` so `pty up` /
  `pty gc` treats the CoS as durable — pty resurrects it if its
  daemon dies, and idle-cleanup won't reap it.
- Boots Claude Code with `--resume` semantics tied to the session id
  it just recorded.

The CoS opens in your terminal — now knowing it's a CoS.

### Alternative: `--ding` (MCP-hostile environments)

If your environment can't run MCP servers at all — sandboxed
runners, some corporate-managed setups, or specific Claude Code
distributions where MCP is disabled — add `--ding` to the launch:

```sh
st launch claude --identity cos --permanent --ding \
  --permission-mode bypassPermissions \
  --persona ~/src/github.com/myobie/personas/chief-of-staff.md
```

`--ding` swaps the MCP-based delivery path for the same
codex-style pattern: no `.mcp.json`, no channel-injection, plus a
`pty send`-based ding sidecar that delivers `[DING] `-prefixed
notices into the CoS's terminal on each new inbox message. The
CoS then uses the `st` CLI (`st message ls / read / reply /
archive`) for all bus ops.

`st launch --ding` also installs a `DING-BUS.md` file next to
`PERSONA.md` and wires it into `CLAUDE.md` (via
`@DING-BUS.md`) — the ding-mode analog of the bus-mechanics
instructions the MCP server would otherwise send as
`instructions:`. So a ding-mode CoS knows the CLI flow,
`[DING]`-poke handling, and the "threads stay on the bus"
convention without needing MCP.

Behavior guarantees:
- Boot ritual / PreCompact flush / StopFailure ding hooks still
  generate — those are Claude Code hooks, MCP-independent.
- Spawner-shaped detection still applies (cos + supervisor
  default `bypassPermissions`, warn without `--permanent`).
- Hooks-not-found emits the same LOUD stderr banner as MCP-mode
  when the shipped `examples/claude-code/hooks/` isn't on disk.

## 3. What the CoS does on boot (from the persona)

The `chief-of-staff.md` file the launch just installed tells the
fresh agent its mission and its bootstrap sequence. Concretely, on
first boot the persona instructs the agent to:

- **Run the first-run interview** — the persona references its
  sibling `first-run-interview.md` (in the same `personas` checkout
  you cloned in step 1) and walks you through the setup covered in
  the next section. On subsequent boots, if the private cos repo is
  already populated, the interview is skipped.
- **Consult the sibling personas as needed** — `manager.md`,
  `specialist.md`, and the others are reference material the CoS
  reads when it spins up a peer agent for you. Same checkout; same
  branch. The CoS is designed against a specific personas commit,
  so pin `myobie/personas` to that SHA when you want reproducible
  behavior across machines (`cd ~/src/github.com/myobie/personas
  && git checkout <sha>`); pull main when you want the latest.
- **Own its own repo** — everything the CoS writes about you and
  your work lives in the cos folder (`context/now.md`, decisions,
  etc.). The personas repo is READ-only reference; the cos repo is
  the private, per-user state.

You don't need to do anything for this step — the persona file the
launch installed drives it. The interview + readiness steps in the
next section are what the CoS actually walks you through.

## 4. First run — interview + readiness

The very first time a CoS boots on a machine (nothing under
`$ST_ROOT/cos/context/` yet), it runs a **first-run interview**:

- **Identity** — your name, handle, timezone, working hours.
- **Repos** — which projects the CoS is aware of; where they live
  on disk.
- **Priorities** — what you're working on right now.
- **Team** — who else you coordinate with (peer humans, other
  agents).
- **Channels** — how you like to be reached (coord messages,
  system notifications, terminal drop-ins).

The CoS writes the answers into your private cos repo — `context/now.md`
and a few sibling files — so a compaction or fresh session picks
them back up on the next boot.

Then it runs a **readiness check** via
[st-evals](https://github.com/myobie/st-evals) — a capability-gated
hermetic smoke suite that preflights what tools you have installed
and then only runs the scenarios your setup can actually support.
The CoS clones st-evals into a scratch dir and runs, from the cloned
repo root:

```sh
bin/st-evals readiness
```

`readiness` verifies the bus works, an agent spawns correctly, and
messages route end-to-end — the minimum viable "your machine can do
the work the CoS will ask of it" gate. Two auxiliary probes you can
run yourself if the CoS reports a miss:

- `bin/st-evals preflight` — lists installed capabilities and which
  scenarios can run given your setup.
- `bin/st-evals list` — the full catalog.

st-evals also honors `PERSONAS_DIR` (or run `bin/ensure-personas.sh`
to fetch a pinned copy) if you want to point it at a local mirror
of the personas repo instead of re-fetching.

## 5. Operating

Once first-run finishes, you talk to your CoS. It manages its own
status, drains its inbox, receives messages from peer agents you
launch later, and coordinates work back to you. Cross-tree
supervision is via `st watch --all` in a second terminal if you
want the raw event stream.

Cold-start recipes to keep handy:

- **Talk to your CoS from another terminal:** `echo "…" | st message send cos`
- **See what your CoS is thinking about:** `st context read cos`
- **Cross-tree overview of everyone:** `st overview`
- **Resume a suspended CoS session:** `cd` back into the cos repo
  and rerun the same launch command (including `--persona`,
  `--permanent`, and `--permission-mode bypassPermissions`) — the
  session id in `.claude-session-id` is what makes it a resume,
  not a fresh start. Re-passing `--persona` is safe: `PERSONA.md`
  gets overwritten with the same bytes, and the `@PERSONA.md`
  line in `CLAUDE.md` is idempotent. `--permanent` re-writes the
  same tag into `pty.toml` (or leaves it as-is if the file
  already exists). The permission-mode flag is idempotent too.

## Bus basics — hand-wiring an identity

Skip this if the CoS quickstart above got you where you needed to
go. This is the direct-CLI path — useful for building tooling on
top of smalltalk, or wiring a non-CoS agent (an eval harness, a
worker scoped to one repo, etc.).

### Pick an identity

An identity is a short lowercase name (letters, digits, hyphens,
periods, 1–32 chars). Conventional shapes:

- A human: your handle (`alice`, `myobie`).
- An agent scoped to one repo: `<repo>-claude` (Claude Code) or
  `<repo>-codex` (Codex). See [repo-ownership.md](repo-ownership.md)
  for the why.
- A cross-cutting coordinator: a bare descriptive name (`cos`,
  `oncall`).

Reserved words are rejected: `inbox`, `archive`, `status`, `name`,
`available`, `busy`, `away`, `dnd`, `offline`, `unknown`, `members`,
`overview`.

```sh
export ST_AGENT=alice   # or legacy ST_IDENTITY / COORD_IDENTITY
```

### Create the identity folder

```sh
st status --set available
```

That single command lazy-creates `$ST_ROOT/alice/{inbox,archive}` and
writes `available` to the status file. You're now visible to peers
as a member of the network. Verify with `st members`.

### Send and receive a message

```sh
echo "hi" | st message send <peer> --subject hello
ST_AGENT=<peer> st message ls
ST_AGENT=<peer> st message read <filename>
ST_AGENT=<peer> st message archive <filename>
```

### Wire MCP for an agent

If you're setting up an agent (Claude Code, Codex, or any MCP host)
and NOT using `st launch`, register smalltalk as an MCP server in
the agent's working repo:

```sh
cd /path/to/repo
st init
```

That writes (or merges into) the repo's `.mcp.json` with a
`smalltalk` server entry pointing at your local `bin/st`.
Idempotent. Channel mode (push notifications on new inbox arrivals)
is on by default; opt out per-repo with `st init --no-channel`.

### Multi-machine sync

Single-machine setups skip this. For two machines to share the same
network, point them at the same identity tree via rsync:

```sh
# On machine A — push to peer
st sync push --all

# On machine B — pull (cron this)
st sync pull --all
```

`st sync sweep` enforces the LAYOUT tombstone invariant — see
[LAYOUT.md](../LAYOUT.md) for the full sync semantics.

## What you don't need

- **No central server.** The filesystem is the API. No daemon to
  authenticate to, no broker to configure.
- **No schema registration.** New message types are just YAML
  frontmatter; readers tolerate fields they don't know.
- **No identity provisioning ceremony.** `st launch` (with a CoS) or
  `st status --set available` (bare) is the whole provisioning step.
  No password, no key, no token.
- **No roster file.** `st members` walks `$ST_ROOT` and enumerates
  everyone present.

## Where to next

- **What's actually happening under the hood:**
  [actor-model.md](actor-model.md) — agents are actors, folders are
  mailboxes, sends are file writes, "no cross-identity edits" is the
  encapsulation rule.
- **The data shape in detail:** [LAYOUT.md](../LAYOUT.md).
- **A three-participant worked example:**
  [walkthrough.md](walkthrough.md).
- **Embedding smalltalk into a TUI or app:** see the "Programmatic
  API" section of the [README](../README.md).
- **Non-CoS agents (workers, specialists, roles):**
  [agent-roles.md](agent-roles.md).

## Troubleshooting

- **`st` isn't on `$PATH` after `npm link`:** run `npm bin -g` to
  find where global npm shims live; add that dir to your `$PATH`
  in `~/.zshrc` / `~/.bashrc` and reopen the shell.
- **`st members` doesn't show you:** confirm `ST_AGENT` (or legacy
  `ST_IDENTITY` / `COORD_IDENTITY`) is set, and that you ran `st
  status --set available` at least once — the identity folder is
  created lazily.
- **`unknown` status next to your name:** you wrote status more
  than 15 minutes ago and no MCP server / `st ding` has refreshed
  the mtime since. Either run `st status --set available` again,
  or leave the MCP server running to keep it fresh.
- **A peer's machine doesn't see a message you sent:** rsync hasn't
  delivered yet. Run `st sync push --all` on the sender, or `st
  sync pull --all` on the receiver. The filesystem is the transport;
  delivery follows whatever cadence you sync on.
- **The CoS won't boot / hook script errors:** check
  `.claude/settings.local.json` in the cos repo — every hook
  `command:` should start with `ST_BIN=/absolute/path/to/bin/st`
  followed by the absolute path to the hook script. If those paths
  drift (you moved the smalltalk checkout), delete
  `.claude/settings.local.json` and re-run the launch command to
  regenerate.
- **The CoS boots but acts like generic Claude (no first-run
  interview, no CoS awareness):** you probably launched without
  `--persona`. Bare `st launch claude --identity cos` produces a
  Claude Code session with no persona wired. Confirm: in the cos
  repo, check that `PERSONA.md` and `CLAUDE.md` exist and that
  `CLAUDE.md` contains an `@PERSONA.md` line. If they're missing,
  re-run the launch with `--persona
  ~/src/github.com/myobie/personas/chief-of-staff.md`.
- **The persona path in `--persona` didn't resolve:** verify the
  personas repo was cloned in step 1 (`ls
  ~/src/github.com/myobie/personas/chief-of-staff.md`). If you
  cloned it elsewhere, use that absolute path in `--persona`.
- **`st launch` warns "launching a CoS without --permanent":** you
  omitted `--permanent` from the launch command. A CoS is your
  always-on center — without the flag, the generated `pty.toml`
  doesn't tag the session as permanent, so `pty gc` may reap it
  under idle-cleanup. Re-run the launch WITH `--permanent`; the
  regenerated `pty.toml` will carry `strategy = "permanent"`.
- **CoS disappeared from `pty ls` overnight:** likely the
  reap-able-CoS failure — the launch was missing `--permanent`.
  Delete `pty.toml`, re-run the launch with `--permanent`, and
  `pty up` will bring it back with the correct tag baked in.

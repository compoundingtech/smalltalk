---
date: 2026-07-05
audience: a new human setting up smalltalk for the first time
status: living doc — update as the surface evolves
---

# Onboarding a new participant

Smalltalk is Nathan's coordination bus for humans and agents — the
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

Smalltalk depends on `@myobie/pty` via a `file:` link, so clone both
side-by-side:

```sh
mkdir -p ~/src/github.com/myobie && cd ~/src/github.com/myobie
git clone https://github.com/myobie/pty
git clone https://github.com/myobie/smalltalk
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

**Pick a directory that will become your private cos repo.** `st
launch` writes identity + hook wiring into the cwd, and marks its
own infra files as git-excluded via `.git/info/exclude` — so it
wants the cwd to be a git repo. If it isn't yet, `git init` first.

```sh
mkdir ~/src/github.com/<you>/cos && cd ~/src/github.com/<you>/cos
git init
st launch claude --identity cos
```

If your `claude` binary is aliased (`cl1`, `cl2`, etc.), pass it
explicitly:

```sh
st launch claude --identity cos --agent cl1
```

What `st launch` does for you:

- Registers the `cos` identity in `$ST_ROOT/cos/{inbox,archive}` and
  writes `available` to its status file.
- Writes `.mcp.json` in the cwd pointing at the smalltalk MCP
  server (channel mode on by default).
- Writes `.claude/settings.local.json` with the three hooks wired up
  and `ST_BIN=<absolute path>` baked in so the hooks are robust to
  PATH drift.
- Generates a `pty.toml` so the CoS runs under `pty up` supervision
  if you've installed the pty tool.
- Boots Claude Code with `--resume` semantics tied to the session id
  it just recorded.

The CoS opens in your terminal.

## 3. The CoS consumes the `personas` repo

On first boot, the CoS clones (or fetches) the public **personas**
repo at https://github.com/myobie/personas as its role contract —
what "Chief of Staff" means, how to act, what conventions to follow.
The reference is **SHA-pinned** at the personas commit the CoS
persona was designed against, so upgrades are deliberate rather
than incidental. You don't have to do anything for this step; it
happens inside the CoS's boot ritual.

If you're offline or want to mirror personas locally, clone
https://github.com/myobie/personas into your git tree; the CoS will
find it via the same path convention it uses for its own repo.

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

Then it runs a **readiness check**: clones `st-evals` and runs the
basic set for the tools you've installed (Claude Code, pty,
smalltalk itself), confirming your machine can actually do the work
the CoS will ask of it before you rely on it. The exact
`st-evals` invocation will be documented here once st-evals ships
publicly — for now, the CoS handles the fetch and run internally.

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
  and rerun `st launch claude --identity cos` — the session id in
  `.claude-session-id` is what makes it a resume, not a fresh start.

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
  `.claude/settings.local.json` and re-run `st launch claude
  --identity cos` to regenerate.

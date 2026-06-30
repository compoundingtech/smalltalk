# Claude Code integration — smalltalk session-boundary hooks

This directory has the pieces for plugging smalltalk into Claude Code
at the **session boundary** layer: a `SessionStart` hook that runs
the boot ritual on every cold start / resume / `/clear` / `/compact`,
and a `StopFailure` hook that surfaces API-error wedges to myobie via
smalltalk so a quiet, wedged session doesn't go unnoticed.

## What's in here

- `hooks/session-start.sh` — minimal Claude Code `SessionStart` hook.
  Wakes the agent with a system reminder telling it to run the
  smalltalk boot ritual (status → available, drain inbox).
- `hooks/stop-failure.sh` — Claude Code `StopFailure` hook. Fires when
  a session ends mid-turn due to an Anthropic API error; branches by
  `error_type` and either sets the agent's status (away/offline) and
  optionally `st message send`s myobie a tuned notice — or stays
  silent for programmer-error types. See **StopFailure** below.
- `settings.local.example.json` — full example settings file with
  both hooks wired up. Copy to `<agent-repo>/.claude/settings.local.json`
  and swap the absolute paths for your smalltalk checkout.

## Why it's needed

Channel mode (`st mcp --channel`) handles **new arrivals during a
session**: the chokidar watcher emits `notifications/claude/channel`
frames and the SDK surfaces them as `<channel source="coord" …>`
blocks in the agent's context, triggering a turn. (The `source="coord"`
attribute is kept through the rename window for back-compat with
downstream parsers; the Phase 5 cleanup flips it to `source="st"`.)

What it doesn't handle is **start-of-session**. When Claude Code boots
fresh, or resumes a saved conversation (`--resume`, `--continue`,
`/resume`, `/clear`, `/compact`), the MCP server reconnects and the
ritual instructions are loaded — but if nothing new lands in the
inbox, the agent never gets a turn, so the ritual never runs. Result:
the agent stays silent with stale status and an unread inbox.

This hook closes that gap. The `SessionStart` hook fires on every
session boundary listed above; with `asyncRewake: true` the script
runs in the background and Claude Code injects its stderr as a system
reminder, which counts as a turn-triggering event. The agent reads
the reminder, runs the ritual, and the session is in a known-good
state from the first turn.

## Install

1. Drop the hook somewhere stable. The default if you cloned this
   repo is `<this-repo>/examples/claude-code/hooks/session-start.sh`;
   copy or symlink it wherever you prefer.

2. Add both hooks to either `~/.claude/settings.json` (user-scope,
   every Claude Code session on the machine) or
   `<repo>/.claude/settings.local.json` (project-scope, just one repo).
   `settings.local.example.json` in this directory is the full
   reference shape; the short form is:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
             {
               "type": "command",
               "async": true,
               "asyncRewake": true,
               "command": "/absolute/path/to/smalltalk/examples/claude-code/hooks/session-start.sh"
             }
           ]
         }
       ],
       "StopFailure": [
         {
           "hooks": [
             {
               "type": "command",
               "command": "/absolute/path/to/smalltalk/examples/claude-code/hooks/stop-failure.sh"
             }
           ]
         }
       ]
     }
   }
   ```

   Use absolute paths — Claude Code does not resolve `~` or
   repo-relative paths in hook commands. The `StopFailure` hook
   doesn't need `asyncRewake` (it doesn't need to inject a turn into
   the wedged session — it just records what happened).

3. (If you're using channel mode for push, which you probably are:)
   make sure Claude Code is launched with
   `--dangerously-load-development-channels server:coord` so the
   `<channel source="coord">` blocks surface as visible turns. The
   `server:coord` token is the MCP server's announced name when
   invoked via `bin/coord`; Phase 5 of the rename flips it to
   `server:st`. `experimental.channelsEnabled: true` in Claude Code's
   settings is the durable form of the same opt-in.

## How it relates to channel mode

The two are **complementary**, not redundant.

| When | What fires | What runs |
|---|---|---|
| Session boundary (cold start, resume, /clear, /compact) | `SessionStart` hook → system reminder | Boot ritual (status set, inbox drain) |
| Mid-session new inbox file | `notifications/claude/channel` MCP push | Read + reply + archive the specific message |

If you only run the hook, channel pushes still work — but you'd miss
real-time pings during the session. If you only run channel mode,
pushes work but resumed sessions stay silent until something new
arrives. Both together is the intended setup.

## StopFailure — surfacing API-error wedges

Claude Code emits a `StopFailure` hook event whenever a session ends a
turn because of an API error (rate-limit, auth failure, billing,
transient 5xx, etc.). When that happens, the agent stops responding
and there's no obvious signal to a human that anything's wrong — the
session just goes quiet. Multiply by N agents on the machine and you
get the "all-quiet-Anthropic-capacity-wedge" failure: nobody notices
for hours.

`hooks/stop-failure.sh` closes that gap **without** auto-restarting —
myobie's explicit call is "just visibility, not automation." The hook
reads the JSON envelope Claude Code pipes on stdin, parses
`error_type` via jq, and applies a tuned policy:

| `error_type`                                  | What the hook does                                                                                                          |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------|
| `rate_limit`                                  | `st status <id> --set away`. **No** ding (transient — would spam during Anthropic capacity events).                         |
| `server_error`                                | `st status <id> --set away` + standard-priority `st message send myobie` notice.                                            |
| `authentication_failed`                       | `st status <id> --set offline` + **high-priority** `st message send myobie` (needs human intervention).                     |
| `oauth_org_not_allowed`                       | Same as `authentication_failed`.                                                                                            |
| `billing_error`                               | `st status <id> --set offline` + high-priority "billing issue" notice.                                                      |
| `max_output_tokens`, `invalid_request`, `model_not_found` | **Ignored.** Programmer error / long-turn config issue — not infrastructure; the agent owns recovery.           |
| anything else (incl. literal `unknown`)       | `st status <id> --set away` + standard-priority notice with the `error_type` verbatim, so we extend the table after triage. |

The script reads the agent's identity from `$ST_AGENT` (or the legacy
`$ST_IDENTITY` / `$COORD_IDENTITY` fallbacks), which should be set in
the session env (e.g. for a pty-supervised Claude Code session, set
it in `pty.toml`'s `[sessions.claude.env]` block).

**Why a single script with internal branching**, rather than one
matcher-entry per `error_type` in `settings.local.json`? The policy is
likely to evolve as we see new wedges in the wild; keeping the
branching in the script means tuning the table is a one-file edit,
not a settings.json migration across every agent's repo.

**Not in scope here**: wedge detection cron, auto-restart, or any
auto-recovery. This is visibility only. A human sees the ding, looks
at `st agents`, and decides whether to nudge or wait.

## How it pairs with the other harnesses

- **Pi** has an analogous `pi.sendUserMessage` call inside
  `examples/pi/coord.ts`'s `session_start` handler. Same idea: force
  a turn on every session boundary so the boot ritual runs. (The
  extension filename `coord.ts` is preserved for back-compat with
  installed pi extensions; new installs can use any filename.)
- **Codex** doesn't have an asyncRewake equivalent. The existing
  `examples/codex/session-start.sh` hook already runs on each new
  session and injects the inbox snapshot as `additionalContext`,
  which serves the same nudge purpose for that harness.

## Boundaries

- The hook **doesn't shell out to `st`** itself. The boot ritual
  is documented in the MCP server's `CHANNEL_INSTRUCTIONS` string and
  the smalltalk SKILL.md — by the time the agent reads the system
  reminder, those instructions are already loaded. The hook just
  triggers a turn.
- The hook **doesn't auto-set status** or auto-drain the inbox. That's
  the agent's job, per the ritual. Doing it from the hook would
  collide with the agent's own actions and break the
  one-writer-per-agent rule.
- This script is **per-machine, per-user setup**. It's not bundled
  into anyone's automatic install path — you copy/symlink + edit
  settings.json yourself.

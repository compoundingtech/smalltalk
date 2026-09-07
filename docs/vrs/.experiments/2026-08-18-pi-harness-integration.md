# pi as a third st2 harness — measured integration surface

Date: 2026-08-18. Status: experiment record. Nothing implemented in `src/`; no declaration published.

Subject: `pi` — the pi coding agent CLI, npm `@earendil-works/pi-coding-agent` **0.84.2**, repo
`earendil-works/pi`. The older `@mariozechner/pi-coding-agent` (0.73.1) is npm-deprecated in favour of
it. `pi` was not on this host's `PATH`; 0.84.2 was installed into a scratch prefix for these runs.

## Method

All runs used a local fake OpenAI-completions server rather than a real provider, so every result is
reproducible with no credentials and no network. Artifacts are in
[`2026-08-18-pi-captures/`](2026-08-18-pi-captures/): the prototype extension
(`st2-channel.ts`), the fake provider registration (`fake-provider.ts`), the fake model server
(`fake-llm.mjs`), and the two event captures cited below.

## Question

st2's two existing harnesses each solve native delivery a different way, and neither is cheap:

- Claude: an MCP stdio child (`src/claude_mcp.rs`) plus an outer session wrapper that owns presence
  because Claude may close that child (`src/claude_session.rs`), plus lifecycle hooks, plus
  workspace-trust config mutation (`src/pretrust.rs`).
- Codex: a dedicated app-server daemon, an observer connection opened before the interactive client,
  thread-ownership binding, and a hard version pin — 4940 lines in `src/codex_app_server.rs`.

pi's extension API changes what is available. An extension runs **inside** the interactive TUI
process, can inject a user message, and sees a full lifecycle event stream. The measurements below
were taken to decide whether that is real.

## Result

| Fact | Evidence |
|---|---|
| An out-of-process file drop reaches a **live interactive** pi TUI and drives a real model turn | `run-a-idle-delivery-and-sigterm.jsonl`: `fs.watch` at `…984121` → `deliver.ok` same millisecond → `agent_start` at `…984122` → `agent_settled` at `…984179`. End-to-end 58 ms. |
| Delivery mid-turn is a **typed queue**, not a race | `run-b-midturn-delivery-and-sigkill.jsonl`: drop at `…001869` while `idle:false`; `sendUserMessage(…, {deliverAs:"followUp"})` accepted immediately; `pending:true` appears at `turn_end` `…012927` and the queued message becomes its own `turn_start` in the same millisecond. |
| pi exposes a positive idle proof to the delivering process | `ctx.isIdle()` is `false` from `agent_start` through `agent_end` and `true` at `agent_settled`, in both captures. No screen inspection involved. |
| Graceful death is **observable** | SIGTERM to pi emits `session_shutdown` (`run-a…`, `…988202`) and pi then exits. Claude emits nothing on SIGTERM (`2026-08-17-captures/claude-hooks/run4-sigterm.ndjson`). |
| SIGKILL death is **silent**, exactly as for Claude | `run-b…` ends at `agent_settled`; no further record after SIGKILL. A heartbeat/staleness model is still required; pi buys nothing here. |
| The project-trust dialog blocks startup, and `-a` clears it without touching config | A workspace containing `.pi/settings.json` renders a five-option "Trust project folder?" modal and no event fires at all — not even `session_start`. Re-run with `pi -a`: `session_start` at `…943097`, no modal. `~/.pi/agent/trust.json` was never created. |
| Extension selection is precise | `pi --no-extensions -e A -e B` loaded exactly A and B; the host's three ambient global extensions were excluded. |
| A modal does not swallow or corrupt delivery | With the `/model` picker open, `isIdle()` reads true, `sendUserMessage` succeeds, and a full turn runs while the human's modal stays untouched; dismissing it reveals the exchange in the transcript. `run-c-delivery-while-modal-open.jsonl`. |
| The production shape works, not just the isolated one | Ambient global extensions left enabled and st2's channel merely added with `-e`: all five loaded and delivery behaved identically. `run-d-ambient-extensions-production-shape.jsonl`. This is the configuration st2 actually uses — it does not pass `--no-extensions`, which would disable the operator's own pi setup. |
| pi runs in its own process group under `pty` | `pty` daemon pid 2347433 (pgid 2347433); `pi` pid 2347534 (pgid 2347534). |
| Sessions are an append-only ISO-timestamped JSONL tree | `~/.pi/agent/sessions/<slugged-cwd>/<ts>_<uuid>.jsonl`; entry types `session`, `model_change`, `thinking_level_change`, `message`, each with `id`/`parentId`. Survives SIGKILL as a last-activity record; carries no death signal. |
| pi has **no MCP** | Stated design position in the shipped README ("No MCP"). The Claude-shaped `deliver "mcp"` transport has no pi analogue. |
| The pi composer has no distinctive marker | It renders as a full-width `─` rule, the editor line, and a second `─` rule (`run-b-screen.txt`, and `cat -v` of the raw peek). Nothing comparable to Codex's ANSI composer markers or Claude's prompt glyph. |

## What each fact removes from the integration cost

- No app-server daemon, no observer pre-connection, no thread binding, no protocol version pin: the
  channel is in-process and the injection point is a documented API call.
- No screen scraping on the delivery path. The synchronous-proof rule in
  [`0001-ding-harness-dispatch-is-positional-and-harness-owned`](../.decisions/0001-ding-harness-dispatch-is-positional-and-harness-owned.md)
  governs PTY writes; a natively-delivered agent never enters that path
  (`crates/agent-spec/src/spec.rs:887` refuses `ding` together with `deliver`).
- No `pretrust.rs` analogue: `-a` is a launch flag, so nothing mutates ambient user config and the
  multi-spawn lost-update race that motivated batching for Claude cannot arise.
- Presence still needs a liveness owner, because SIGKILL is silent. This is unchanged from Claude.

## Conclusion

The design these measurements support is implemented on this branch and recorded as
[decision 0005](../.decisions/0005-pi-delivers-natively-through-an-injected-extension.md). One
end-to-end run against a real catalog is captured verbatim in
[`run-e2e-catalog-delivery.txt`](2026-08-18-pi-captures/run-e2e-catalog-delivery.txt): `st2 hooks
install` publishes a set containing `pi-channel.ts`; a typed `pi {}` declaration expands to a
`pi-session` launch that names no machine-local path; `st2 up --once` starts it; the extension spawns
`st2 driver pi-channel` as the *exact* control-plane binary; a real `st2 message send` arrives in the
live session as `Subject: deploy check` and drives a turn; presence carries a v1 heartbeat; and
`st2 down` reaps the session and its channel child.

A second capture,
[`run-f-session-replacement-single-channel.txt`](2026-08-18-pi-captures/run-f-session-replacement-single-channel.txt),
covers the case the extension has to get right on its own: `session_start` also fires for `/new`,
`/resume`, and `/fork`, and a second channel child would replay every unread message, because each
channel keeps its own delivered set and neither would suppress the other's. After `/new` there is
exactly one channel child — a *different* pid from the one at boot, so the predecessor was closed
and a successor opened — and a message sent afterwards appears exactly once.

Getting there took three measured corrections, each of which had shipped silently on a reading of
the docs alone:

- pi fires `session_shutdown` around the successor's `session_start` during a replacement, so a
  teardown handler that closed "the current channel" reaped the channel the new session had just
  opened. Delivery stopped completely after `/new`.
- pi **re-instantiates extensions** on session replacement. Configuration read into module scope and
  then removed from the environment was therefore absent on the second instantiation, which ran as
  an unmanaged session and never opened a channel at all.
- for the same reason, channel ownership cannot be instance-local: with a per-instance handle, `/new`
  left two channels alive against one inbox, each with its own delivered set. Ownership is
  process-wide, so opening a channel closes its predecessor whichever instance opened it.

None of this is visible in pi's documented lifecycle. It is the reason the session-replacement case
is captured rather than assumed.

## Measured after the design was fixed

Three behaviours were added on review and each was verified, not assumed
([`run-g-session-restore-and-env-leak.txt`](2026-08-18-pi-captures/run-g-session-restore-and-env-leak.txt)):

| Fact | Evidence |
|---|---|
| Restored context reaches the **first** model call, not the one after it | The extension awaits the channel's `hello` inside `session_start`, so restoration is ordered ahead of the boot turn. Request 1 is `roles: [system, user, user]` with `hasRestoredContext: true`. Injecting on a later event would have missed the boot turn entirely, which is the failure the Codex and Claude session-start hooks exist to prevent. |
| st2's channel variables do **not** reach pi's tool children | A real `bash` tool call reports `LEAK_CHANNEL=0`. The extension reads `ST2_PI_CHANNEL_*` once and unexports them, so a nested pi cannot inherit its launcher's bus identity or inbox. |
| pi's **own** session variables **do** reach tool children | The same call reports `PI_SESSION_ID_SET=yes`. This is the measured justification for adding `PI_*` to the eval seat scrub beside the existing `CLAUDE_*`/`CODEX_*` entries — it is the identical leak class, not a speculative one. |
| A real tool-call round trip works end to end | Request 2 is `roles: [system, user, user, assistant, tool, user]`. This closes the "no tool-call capture" gap noted below for the delivery path specifically; it does not close the live-provider gap. |

## What is not yet established

- ~~**No live provider run.**~~ **Closed.** See
  [`run-h-live-provider.txt`](2026-08-18-pi-captures/run-h-live-provider.txt): boot with restored
  context (proven with a nonce the model read back), idle delivery, real tool calls, and two
  mid-turn steers, all against a live remote model. Two things it added that the fake server could
  not: the steer boundary is finer than documented — a steer waits out only the *in-flight* tool
  call, landing in the same millisecond as its result (measured at 10s and 45s tool-call lengths) —
  and the free tier rate-limits per model, so rotating models beats backing off. Compaction remains
  unexercised (context stayed under 10%). The
  requested credential (OpenCode Zen, `OPENCODE_API_KEY`) was not present on this host
  (`~/.pi/agent/auth.json` is `{}`; no key in the environment or in `~/.config/opencode`).
- **No blocked-on-human case.** pi's permission gating is extension-implemented rather than built in,
  so the Codex `activeFlags` analogue has not been located, and no capture shows a pi session waiting
  on a human. The trust modal is a *different* blocking state, and the measurement above shows it is
  invisible to extensions — which is itself the interesting result.
- **The restored ritual is a pointer, and the thing it points at is load-bearing.** The boot ritual
  names no commands; `templates/bus.st2.md` does. Live, with that contract in the workspace, the
  model set its status and drained its inbox to zero using the documented commands. Without it, the
  identical prompt sent the model hunting the filesystem with `find /`, and it never set its status.
  This is not pi-specific — a Codex seat declared without the contract fails the same way — but it
  is the reason `examples/native/agent-pi.kdl` renders the shipped contract directly. Replicated
  independently in two concurrent runs by operators not reading each other's work; the second also
  observed the `busy` transition. The contract *verbatim* was enough for a weak free-tier model, so
  no operator-composed persona is required.
- **Version durability is now measured, and answered.** Across pi 0.74.0..0.84.2 — 41 releases, 40
  transitions — the whole `types.d.ts` changed in 17 transitions while the surface this extension
  depends on changed in exactly one, additively (`expandPromptTemplates?` added to
  `sendUserMessage`). pi publishes no changelog, stability policy, compat field, or API version
  constant; its declarations are the only artifact that governs the coupling, so
  `checks.pi-extension-types` type-checks the extension against a pinned release. `tsc --noEmit`
  passed on all 41 sampled versions and failed on each simulated break: a wrong `deliverAs` value, a
  wrong event name, and — the one that matters — using the idle proof as a property instead of
  calling it, which is the silent failure that would turn every mid-turn delivery into a plain
  send.

## VRS Impact

The measurements ground the pi native-delivery specification, its in-process
idle proof, session-replacement channel ownership, and type-checked extension
boundary.

## Host context worth recording

This host already runs pi under a separate agent-management system: two global extensions
(`~/.pi/agent/extensions/nix-managed-pty-events.ts`, `nix-managed-status-footer.ts`) emit
`user.agent.model` / `user.agent.status` into the `pty` user-event stream via `agent-pty-emit`, and
the footer already renders `$ST_AGENT` — st2's own runner-owned identity variable. Any pi integration
lands beside that, not on empty ground: `pty emit user.*` is an existing, deployed out-channel from a
pi process, and st2 should decide deliberately whether to use it or to keep its own.

# pi delivers natively through an st2-injected extension, and never through DING

Status: accepted

## Context

st2's two maintained harnesses reach a live interactive session in two different, expensive ways.
Claude gets an MCP stdio child plus an outer wrapper that owns presence because Claude may close
that child, plus lifecycle hooks, plus a batched mutation of the operator's ambient config to clear
the workspace-trust dialog. Codex gets a dedicated app-server daemon, an observer connection opened
before the interactive client, thread-ownership binding, and a hard version pin — 4940 lines.
Underneath both sits DING, the screen-scraping transport that exists because neither harness offered
anything better at the time, and whose safety rests on a synchronous adjacent composer proof
([`0001-ding-harness-dispatch-is-positional-and-harness-owned`](0001-ding-harness-dispatch-is-positional-and-harness-owned.md)).

Adding pi asks which of these shapes it should take. pi answers the question itself: it has no MCP,
no app-server, and no lifecycle-hook mechanism. What it has is an extension API that runs **inside**
the interactive process.

## Evidence and Argument

Measured on `@earendil-works/pi-coding-agent` 0.84.2 against a local fake provider, so every run is
reproducible with no credentials. Full record and captures:
[`../.experiments/2026-08-18-pi-harness-integration.md`](../.experiments/2026-08-18-pi-harness-integration.md).

- **The delivery path is evented, not observed.** An out-of-process inbox drop reached a live
  interactive TUI and drove a complete model turn in 58 ms: `fs.watch` → `sendUserMessage` in the
  same millisecond, `agent_start` one millisecond later, `agent_settled` 57 ms after that. Nothing
  inspected a screen.
- **Mid-turn delivery is a typed queue, not a race.** Dropped while `isIdle()` was false, the message
  was accepted immediately and became its own turn in the same millisecond the running turn ended.
  This is the case DING can only defer and retry.
- **The idle proof is positive and in-process.** `ctx.isIdle()` is false for exactly the span
  `agent_start`..`agent_end`. Compare the defect the prior research found in the PTY observer, where
  a Claude pane proves idle exactly once and is unprovable forever after.
- **A modal does not corrupt delivery — and this was the case most likely to sink the design.** With
  pi's `/model` picker open, `isIdle()` reads true, the message is delivered, a full turn runs, and
  the human's modal is untouched; dismissing it reveals the exchange in the transcript. This is the
  pi analogue of the Codex defect where `activeFlags: waitingOnApproval` is discarded and st2 steers
  into an open approval dialog, and pi does not reproduce it.
- **The production configuration was run, not assumed.** With the host's three ambient global pi
  extensions left enabled and st2's channel merely added with `-e`, all five loaded and delivery
  behaved identically.
- **The composer offers nothing to scrape.** pi renders a full-width `─` rule, the editor line, and a
  second rule. There is no Codex-style ANSI composer marker and no Claude-style prompt glyph.
  [`0001`](0001-ding-harness-dispatch-is-positional-and-harness-owned.md) requires evidence from a
  real screen before widening what counts as idle; this screen supplies none.
- **pi buys nothing on death.** SIGTERM emits `session_shutdown` — better than Claude, which emits
  nothing — but SIGKILL is silent, exactly as for Claude. Presence still needs an owner and still
  decays by staleness.
- **The trust dialog is real and cheap to clear.** A workspace with `.pi/settings.json` renders a
  blocking modal during which *no extension event fires at all*, not even `session_start`. `pi -a`
  clears it for that run and writes nothing to `~/.pi/agent/trust.json`.

## Options

| Option | Tradeoffs |
| --- | --- |
| A pi DING adapter, like the incumbents' fallback | Reuses the shipped transport and needs no new process. But the composer has no marker to key on, so the idle proof would rest on generic rules — the exact widening `0001` refuses without screen evidence — and it would forgo an evented channel that already exists and measures better. |
| RPC mode (`pi --mode rpc`) as the channel | A documented protocol st2 could speak directly. It *replaces* the TUI, so the human-visible pane under `pty` that st2's whole model assumes would be gone. This is the Codex problem — needing a second connection because the interactive client is opaque — reintroduced by choice. |
| An extension st2 injects, talking to an st2-owned channel process | Requires a new wrapper, a new stdio protocol, and shipping a TypeScript asset in an otherwise Rust binary. In exchange the injection point is a documented API call, the idle proof is in-process, and no screen is ever parsed. |

## Decision

pi is a natively-delivered harness. It declares `deliver "pi-channel"` or a typed `pi {}` driver,
and **no pi arm is added to the DING registry**. The existing rule that refuses `ding` together with
`deliver` (`crates/agent-spec/src/spec.rs`) is what keeps a pi agent off the PTY write path
entirely, so [`0001-ding-harness-dispatch-is-positional-and-harness-owned`](0001-ding-harness-dispatch-is-positional-and-harness-owned.md) is untouched.

Four things follow, each chosen against a specific failure it prevents:

1. **st2 injects the extension; the declaration never names it.** `st2 driver pi-session` resolves
   `pi-channel.ts` from this binary's *verified* immutable set and splices `-e <path>` into the
   provider argv at launch. A path written into the declaration would pin one host's layout into a
   catalog, and an `$ST_HOOKS` token in an argv would resolve to the receipt-bearing root rather
   than the selected set. The asset ships inside the same content-addressed hook set as the Codex
   and Claude lifecycle scripts, because for pi an extension *is* the hook mechanism.
2. **The extension holds no policy.** It moves frames. st2 decides which message is delivered and
   how; `deliverAs` travels on the frame, so changing delivery behaviour is a Rust change and not a
   redeploy of a TypeScript asset. The initial value is `steer` — the earliest point pi accepts
   input without discarding the running turn, and the same choice the Codex native path makes.
   Per-message selection between steer and queue is #277. Measured live, `steer` is a stronger
   guarantee than its own description: the delivery boundary is a single tool call, not the whole
   job, so a message sent mid-job waits only out the in-flight call rather than the work remaining. The same rule governs session start:
   pi has no session-start hook, so what a restarting agent is told is composed in Rust
   (`session_context`) with the Codex hook's exact blocks and order, and rides the `hello` frame.
   The extension awaits that frame inside `session_start`, which is what puts restored context in
   the boot turn rather than the turn after it.
3. **The wrapper owns presence, as it does for Claude.** The extension lives only as long as pi's
   process and SIGKILL is silent, so nothing in-process can be the liveness authority. The wrapper
   also exports its own executable path: resolving `st2` from `PATH` inside the extension would let
   a replaced control plane and its live agents disagree, which is what **R11 control-plane
   replacement safety** exists to prevent.
4. **Trust is a launch flag, not a config mutation.** Expansion emits `pi -a`. There is no pi
   analogue of `src/pretrust.rs`, and therefore no pi analogue of the multi-spawn lost-update race
   that forced trust writes to be batched before the first Claude boot.
5. **A managed seat is offline and does not leak its identity downward.** The wrapper sets
   `PI_OFFLINE=1` and `PI_SKIP_VERSION_CHECK=1` where the operator declared nothing, so a supervised
   agent does not self-update or make boot latency depend on the network. The extension unexports
   `ST2_PI_CHANNEL_*` after reading it, because pi puts its environment in front of every tool
   child; measured, a nested child now sees none of them while pi's own `PI_*` still reaches it,
   which is why the eval seat scrub gains `PI_*` beside `CLAUDE_*`/`CODEX_*`.

## Consequences

- pi is the first harness whose delivery never touches a PTY. **Fail-closed observed native DING**
  and **Bounded DING PTY probe churn** are unaffected — nothing here reaches the phase they govern.
- **Agent-declared presence** gains a third wrapper. The row's wording names the Codex and Claude
  wrappers specifically and now needs pi beside them; the guarantee itself is unchanged, and
  `src/pi_session.rs::idle_pi_provider_refreshes_presence_without_channel_input` proves it.
- A pi agent whose extension fails to load is *unreachable but visible*: presence still decays, so it
  reads as stale rather than as a healthy agent silently dropping mail. This is deliberate. The
  alternative — a wrapper that keeps presence fresh regardless — would make presence lie.
- Adding `pi-channel.ts` to the hook set changes the hookset hash, so every host must run
  `st2 hooks install` to select a set that contains it. `st2 driver pi-session` refuses to launch
  against a set that does not, naming the command.
- st2 now ships a TypeScript asset. It is small, holds no policy, and is content-addressed and
  verified like every other file in the set — but it is a genuinely new kind of artifact in this
  repo and a second language to keep working.
- The extension reports `delivered` and `failed` back on the channel, but st2 does not yet act on
  either: the channel marks a filename delivered when it writes the frame, exactly as the Claude MCP
  channel does, so a `sendUserMessage` that throws is not retried until the channel restarts and
  rescans the inbox. This is parity with the incumbent, not a property of the new design, and the
  ack frames exist so it can be closed without a wire change.
- **pi gets no version pin, because pi publishes no version to pin.** It ships no changelog, no
  stability policy, no compat field, and no extension-API version constant. What it does publish is
  its TypeScript declarations, and those are the artifact that actually governs this coupling — so
  that is what st2 checks. `checks.pi-extension-types` type-checks `hooks/pi-channel.ts` against a
  pinned pi release at build time.

  This follows st2's existing rule rather than inventing one. The repo pins where skew fails
  *silently*: `pty`, whose old builds ignore `--unset-env` and leave a launch looking correct;
  `codex-cli`, whose untyped app-server wire misparses quietly. It does not pin Claude, whose
  failures are loud. The pi extension had exactly one silent surface — `ctx.isIdle?.() ?? true` read
  a missing idle proof as "idle", which would have turned every mid-turn delivery into a plain send
  — and the check catches precisely that: using the idle proof as a property rather than calling it
  is a type error (`TS2774`), demonstrated failing in the Nix build.

  The rate is measured, not assumed. Across pi 0.74.0..0.84.2 (41 releases, 40 transitions) the
  whole `types.d.ts` changed in 17 transitions, while the surface this extension depends on changed
  in **one**, additively (`expandPromptTemplates?` added to `sendUserMessage`). So the check is
  close to noise-free while still refusing a real removal.

  Cost, stated plainly: the check fetches pi's npm tarball (~145 MB unpacked) and `@types/node`, and
  runs `tsc`. It is a separate check derivation, not part of the main build, and it is pinned by
  tarball hash rather than an npm lockfile because pi bundles its sibling packages without integrity
  hashes, which `fetchNpmDeps` cannot express. Moving to a newer pi is a deliberate edit of that pin
  — which is the point.
- The st2-owned channel wire carries its own protocol number that the extension refuses on mismatch.
  That covers st2-side drift and is independent of the above.

## Evidence required before this is load-bearing

- ~~one live-provider run~~ — **done**, and it strengthened rather than weakened the design: the
  steer boundary is one tool call rather than one job, and session restore was proven with a nonce
  the model read back. It also established that the restored ritual only works when the shipped bus
  contract (`templates/bus.st2.md`) is in the workspace, which the maintained pi example now
  requires. Compaction remains unexercised;
- a located blocked-on-human state. pi's permission gating is extension-implemented, so the Codex
  `activeFlags` analogue has not been found, and the one blocking state measured — the trust
  modal — is invisible to extensions;
- an end-to-end run against a real catalog, which the unit-level proofs here do not replace.

# omp is a fifth native driver with its own channel and a hard version gate

Status: accepted; admission policy revised 2026-10-02, explicit operator exception added 2026-10-03 for [#819](https://github.com/compoundingtech/smalltalk/issues/819)

## Context

st2 maintains four typed drivers (claude, codex, pi, opencode), each pairing a pure KDL
expansion with a session wrapper that owns presence, publishes observed harness state, and
delivers inbox messages natively. omp — an earendil-works/pi-family harness in daily use on
this fleet — has none of this: seats are hand-authored tasks with no presence lease, no
observed state, and no delivery path. The question was how deep support should go: a full
native driver, an alias onto the pi driver's machinery, or a documented hand-authored
pattern.

## Options
| Option | Result | Reason |
| --- | --- | --- |
| Full native driver (fifth expansion arm, own wrapper, own channel) | Selected | The 2026-08-25 capture shows the pi mechanism ports and omp's approval events add an axis pi lacks. Cost: ~pi-driver scale code and measured exact-build admission. |
| Alias onto the pi driver | Rejected | Cheapest, but measured divergence makes it wrong: no `agent_settled` means the observed idle edge never fires or blips at `agent_end`, and version gates would pin the wrong harness. Rejected on evidence, not effort. |
| Docs-only hand-authored pattern | Rejected | No presence lease, observed state, or native delivery; inconsistent with all four existing drivers — not "first-class". |
| Blocked axis deferred out of v1 | Rejected | Smaller diff, but omp seats would read busy while actually waiting on a human — exactly what st2's wedged-agent signal exists to catch; both events verified firing (q2). |
| Warn-only version handling | Rejected | omp releases near-daily; silent degradation reads as healthy while a refused launch is loud (q3). |

## Evidence and Argument

Measured against omp v18.0.3 on 2026-08-25; full record:
[`06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md`](../06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md).

- **The pi mechanism ports.** omp loads pi-style extensions; its extension argument carries
  the same `sendUserMessage` / `sendMessage` / `on` calls; a live interactive run delivered
  an idle message end-to-end and drove a complete model turn without touching a screen.
- **But it is not pi.** `agent_settled` — the pi channel's entire idle edge — does not exist
  (absent from the binary); idle must be derived by polling `ctx.isIdle()` after
  `agent_end`. Conversely omp is *richer* than pi where it matters for st2: it exposes
  `tool_approval_requested` / `tool_approval_resolved` with a `toolCallId`, giving the
  blocked-on-human axis pi cannot express at all.

An alias onto the pi driver would bake both divergences into the wrong place: observed state
would hang waiting for an event that never fires or blip idle at the wrong boundary, and
each future divergence would become a special case inside "pi". The measured differences
are exactly why the channel forks rather than shares a file.

## Decision

1. **Full native driver** (`driver omp` block + `omp-session` wrapper + `omp-channel.ts`
   extension + `omp-channel` process), per OMP-R01.
2. **Own channel asset, forked from pi's**, not a shared parameterized file — the idle-edge
   and approval logic differ per harness (OMP-R04).
3. **Publish the blocked-on-human axis in v1** from the two approval events (OMP-R02) —
   verified firing, not speculative.
4. **Hard measured admission of the exact installed build** (OMP-R05). Before taking
   ownership of a live seat, automatically measure the installed executable with the shipped
   channel extension in a disposable RPC session. All five checks must pass: extension load,
   lifecycle event names, a positive idle sample after terminal `agent_end`, a correlated
   requested/resolved approval pair for a harmless isolated tool, and one native message consumed
   in a completed model turn. The channel's `delivered` acknowledgement alone cannot prove
   consumption. A failed or indeterminate check refuses omp launch; it names the boundary in
   the durable driver diagnostic, the Agent API state and `st doctor`.
   Persist results under exact release, executable/installation/interpreter identity, extension
   contents, and probe/adapter implementation identity. A same-version build or extension
   replacement requires fresh evidence. Concurrent launches share a bounded cache lock.
   Scratch HOME, XDG, credential, workspace and PTY roots contain no managed declarations or
   history; the model endpoint is loopback and the empty fixture approval is denied automatically.
   OpenCode uses the same cache discipline and its own API/SSE/permission/completed-consumption
   measurements; failure retains its explicit native-delivery-disabled policy.
   A person may explicitly allow the exact installed executable/interpreter/installation and
   shipped extension with `st admission override omp --binary <path> --reason <reason>` (or
   `opencode`). This exception is a separate local audit record, never a forged pass. It bypasses
   scratch probes, including version probing, for that build; failed measurements remain intact,
   replacement builds/extensions need a new exception, and `st admission revoke` restores the
   normal gate. The named failure diagnostic points to this remedy. This addition follows
   the 2026-10-03 requirement to preserve a person's ability to start their seats when a probe
   cannot run on their machine. It does not automatically downgrade the gate to a warning.
5. **No DING screen adapter for omp** in v1 (OMP-T03), matching the other native-channel
   drivers.

Interview decisions q1–q4 (2026-08-25, Johannes): full native driver; blocked axis included;
hard gate; VRS lives in [`06-omp-driver/`](../06-omp-driver/).

## Consequences

- A fifth expansion arm, wrapper module, channel process, hook asset, and ding launch
  classification follow the existing per-harness pattern.
- Every unseen exact producer/adapter build runs the isolated probe once. Successful new
  versions require no st release or manual allowlist change. Refusals persist too; repairing
  the named producer/adapter boundary and removing its cache record permits a new measurement.
- Existing measured captures remain fixtures for the builds they measured. Admission proves
  the narrow delivery contract, not model/provider matrices, context/token arithmetic, prices,
  every interactive UI mode, or all steer/modal behavior. Those still require their own captures.
- The original per-minor policy is superseded by automatic measurement, as agreed with
  @schickling-assistant on #819. The default loud refusal and all five OMP-R05 checks are preserved;
  a person's deliberate exception is distinguishable from measured admission.
- Admission only gates a new provider launch, including restart and residency resume. Existing
  running providers and driver adoption are unaffected; there is no rollback to an older binary.
  Loopback fixture calls need no real model credentials or Internet, but a producer bootstrapping
  a missing dependency in its empty scratch cache can still fail offline. The explicit exception
  is available in that case; it cannot repair a missing runtime needed by the real producer.
- The deny path, ask-axis discrimination, steer/modal interactions, and update-banner
  suppression remain open (`DQ-OMP-1..5`) and bound v1's claims.

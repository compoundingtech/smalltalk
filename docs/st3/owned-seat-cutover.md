# Owned-seat rollout

A publisher can opt an owned set into draining changed and omitted seats before restarting or
retiring them. st performs and reports the cutover. The publisher still supplies the complete KDL
bundle, an immutable Git SHA, a strictly increasing source sequence and exact set/member fences.
Publication alone does not watch a repository or automatically apply its contents.

```sh
st apply --set garden seats.kdl --repository acme/garden --ref refs/heads/main \
  --sha "$SOURCE_SHA" --source-sequence "$SOURCE_SEQUENCE" --expect-set "$PREVIOUS_SET" \
  --rollout when-idle --rollout-deadline 30m --as agent/garden/publisher
st sets status garden --sha "$SOURCE_SHA" --json
```

The policy is part of the immutable receipt and preview digest. Omitting `--rollout` retains
immediate publication behavior. New seats start normally; unchanged launches and display labels
keep their original launch lineage. Missions and schedules keep their existing semantics, including
preserving active runs after omission.

The initial supported cutover is a top-level native PTY seat on its existing host, harness family and native login account.
Authored session selectors and unsupported native launches are refused before publication. Every
active admitted daemon must advertise seat-rollout support before the policy can be activated.

The owning daemon records an ordinary runtime action with the selected target, receipt, policy,
original incarnation and an absolute deadline. Its phases are `draining`, `stopping`, `starting`,
`verifying`, then `running` or `retired`. New independent messages and new work claims wait in the
graph while it drains. Existing work can renew, report and complete. Replies and the continuation
of a pending person ask remain deliverable. A driver acknowledgment, its typed quiescence report,
native-session binding, pending deliveries, claimed work and running subagents all participate in
the idle boundary. OMP also reports its native async-job count: live background jobs block
cutover, and an unavailable count remains unproven rather than idle.

Replacement renders are prepared without changing the live configuration during drain. Signals
retain the original incarnation fence, and replacement launch waits for positive exit evidence.
The PTY spawn lock refuses an unrelated incumbent and tags the replacement with its operation,
allowing recovery after a lost daemon start receipt. A target or policy changed by a newer winning
receipt supersedes the old operation; identical newer receipts keep its deadline.

The replacement receives a strict native-session selector. Claude transcript carry also applies
when its workspace changed. Success requires the new incarnation, its captured launch token and
the same native conversation binding. A refusal, mismatch or three-minute binding timeout leaves
a failed rollout stopped; it never falls back to a fresh conversation. An ambiguous interrupted
launch remains blocked until its replacement can be identified.

The default drain deadline is thirty minutes. Expiry holds the old seat and releases its intake;
normal reconciliation and restart requests cannot bypass that hold. Retry explicitly with fresh
desired/incarnation fences:

```sh
st agents rollout agent/garden/orchard --deadline 30m --as person/operator
```

`--force-after-deadline` on publication or explicit retry permits interruption of busy work. It
does not override ownership, incarnation, render validity, unknown process identity or an absent
native session. Status includes the forced flag and the blockers overridden. Interrupted work
uses the existing runtime-exit and lease-recovery behavior.

Agent and set JSON reads expose the operation, phase, blockers, deadline and session verification.
Source status distinguishes publication, local visibility, supersession and satisfied rollout.
An omitted seat is retired only after observed exit; mission/schedule publication is not a running
seat. Remote visibility and unreachable owner progress remain unknown without reporting evidence.

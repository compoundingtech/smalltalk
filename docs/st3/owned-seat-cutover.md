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
immediate publication behavior for automatic seats. A seat with `rollout "manual"` still defers
its cutover without an apply-time flag. New seats start normally; unchanged launches and display labels
keep their original launch lineage. Missions and schedules keep their existing semantics, including
preserving active runs after omission.

The initial supported cutover is a top-level native PTY seat on its existing host, harness family and native login account.
Automatic cutovers refuse authored session selectors and unsupported native launches before
publication. Manual publication and adoption can retain the seat's authored `--resume` or
`--continue` selector; they do not request a cutover or prove that the launch can resume.
Every active admitted daemon must advertise seat-rollout support before the policy can be activated.
Manual seats additionally require every active admitted daemon to advertise `seat_rollout_manual`.

## Manual seats

Declare the policy with the seat in the complete owned-set bundle:

```kdl
version 2
agent "garden/orchard" {
  host "amber"
  workspace "/srv/garden/orchard"
  rollout "manual"
  harness "claude" { model "example-model"; }
}
```

The graph applier publishes this declaration with the rest of the fleet using its ordinary
`st apply --set` invocation, including `--rollout when-idle` if the automatic seats should drain.
There is no per-seat apply flag to repeat. The property is in the canonical declaration and
receipt digest, survives retries and subsequent complete publications, and changes through the
same review and fencing as any other declaration field. Omission of the property restores the
set's automatic policy. Adding or removing only the property does not itself change the launch.

Manual owned-set adoption and publication accept a seat's own authored `--resume` or `--continue`
selector without replacing it. Adoption establishes ownership; publication stores the desired
declaration. Neither operation requires cutover resumability merely because the seat is manual.
The other ownership, declaration and publication checks still apply.

When the launch changes, publication leaves the manual seat on its current incarnation and
preserves its live rendered files. Its messages and work intake continue normally; it has no
drain deadline until an explicit rollout is requested. Agent reads show
`published, rollout pending (manual)`; set member reads expose `rollout: "pending"`,
`rollout_mode: "manual"` and the same `publication_status`. This is an intentional deferral,
so the source receipt can have `commit_status.satisfied: true` while `running: false` once
all automatic members have completed their rollout. Failures, unknown progress and active
explicit cutovers still prevent satisfaction. Existing response fields retain their meanings.

```sh
st agents rollout agent/garden/orchard --deadline 30m --as person/operator
```

The explicit command captures fresh declaration and incarnation fences and validates cutover
resumability before recording a durable cutover action or changing intake. The rollout API applies
the same validation. An authored `--resume` or `--continue` selector, or another unsupported
resumption condition, causes refusal at this boundary: the incumbent stays running, its intake
stays open and the published manual change stays pending. Successful validation uses the existing
native drain, resume and session verification flow. The command works even when the set was published
without `--rollout`; its default deadline is thirty minutes. `--force-after-deadline` is an
explicit choice at that command. Ordinary restart, suspend and resume cannot bypass a pending
manual cutover. If the incumbent exits before an explicit rollout, the changed declaration
remains pending; normal crash recovery cannot apply it. An explicit rollout can resume its
recorded native binding after positive exit evidence, with the same declaration and incarnation
fences. An absent binding or unknown exit remains a refusal. A newly added seat starts normally.
Omission of a manual seat publishes its retirement but holds its running incarnation until the
same explicit command retires it.

## Cutover lifecycle

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
`st sets status <set> --sha <sha>` returns 404 until that SHA has a publication receipt; this
is intended, including before a dry run or first apply. Read `st sets show <set>` for the current
set revision used by `--expect-set` fencing.
An omitted seat is retired only after observed exit; mission/schedule publication is not a running
seat. Remote visibility and unreachable owner progress remain unknown without reporting evidence.

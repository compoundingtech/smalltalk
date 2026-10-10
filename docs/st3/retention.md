# Retention

How long st keeps each claim kind is one policy file:
[`crates/st3-schema/retention.toml`](../../crates/st3-schema/retention.toml). It names every
registered claim kind once, with its class, where its writes go and, for a transition, its window.
It also holds the checkpoint rules, in order: which claims of a kind a checkpoint may drop.

The daemon reads the policy when it builds the schema registry (`st3_schema::retention`). It is
compiled into the build rather than read from a node's configuration, because every member of a
fleet must apply the same rules: the rules render the rules description, whose hash is the
`rules_digest` in every checkpoint seal. A member that runs different rules seals different terms
and the checkpoint waits for it, so no member drops a claim its peers keep. See
[Checkpoints and trimming](checkpoints.md) for the agreement, the proof and the trim.

## Classes

| Class | What it holds | How it is kept |
|---|---|---|
| `current` | agent status, harness state, limits readings, leases | the newest claim of each slot; older ones go at the next checkpoint |
| `transition` | status changes and deliveries | for its `window` (seven days), then dropped |
| `durable` | missions, decisions, messages, reviews, evidence, deploy receipts | forever |
| `otel` | observation series: usage samples, latency, readings over time | history goes to OpenTelemetry through `[observations.otlp]`; the graph keeps the latest of each slot |

A person's claim of any kind is durable, whatever its class: the planner never drops one.

## Where writes go

`log` says where a kind's writes go. It is the `retention` that `st3 schema show KIND` prints.

| `log` | Registry retention | Meaning |
|---|---|---|
| `claims` (default) | `durable` | every write is a replicated claim |
| `on-change` | `latest` | every write goes to the local observation log; a claim replicates only when the state changes |
| `local` | `local` | only the local observation log of the node that made it |
| `system-local` | `system-local` | local when the system writes it without an actor; replicated when a person or agent writes it as its actor |

The local observation log is trimmed by each node on its own, after `[observations] retention`
(seven days by default); [Data authority](data-authority.md) describes it. A replicated claim is
dropped only by a checkpoint, which every member agrees to.

## Rules and `pending`

A class says what may be dropped; a rule says how. A checkpoint drops a claim only when a rule for
its kind names a slot, the claim is not the one the rule keeps, a later kept claim of the slot
witnesses it, and no guard protects it. A kind whose class allows dropping but which has no rule
yet carries `pending`, a short reason it is still kept whole: usually that some fold reads every
claim of the kind, so a later claim does not replace an earlier one. A test fails when a kind's
class allows dropping and it has neither a rule nor a `pending` reason, and when a durable kind has
a rule.

Each rule has:

- `kind`, and `when`: conditions that must all hold (`actor=null`, `FIELD=set`, `FIELD=VALUE`);
- `slot`: the fields besides subject and kind that make up a slot. `claim.origin` is the writer.
  A rule without a slot keeps every claim. `require_slot` leaves out a claim missing a slot field;
- `keep`: what the rule keeps. It names the planner function in
  `crates/st3/src/store/checkpoint_rules.rs` (`planner_rule`); the checkpoint tests fail on one
  it does not know;
- `min_age`: only claims accepted at least this long before the cut go.

The first rule of a kind whose conditions hold applies.

## Changing the policy

Changing a class, a window or a `pending` reason changes nothing a checkpoint does. Changing a
rule does, and changes the rules digest:

1. List every reader of the kind and give each kept claim a reason in terms of a reader's answer.
   If the checkpoint proof's reader answers (`subject_answers` in `checkpoint_rules.rs`) do not
   cover the new reader, add it, so a rule that changes an answer fails the proof.
2. Bump `RULES_VERSION` and say why in its comment.
3. Add a test in `crates/st3/src/store/checkpoint_tests.rs` that names the dropped and the kept
   claims.
4. Measure it on a copy of a real store (below), never on a live one.
5. Deploy it to every member. Until every participant runs the same rules, checkpoints wait at
   sealing; replication and ordinary work go on.

## Measuring a change

`crates/st3/examples/retention_plan.rs` plans a checkpoint on a private copy of a store and prints
what it would drop by kind, with the drop and retained digests. With a scratch directory it also
proves the plan, as a real checkpoint does.

```sh
cp /path/to/safety-copy.sqlite3 /private/dir/clone.sqlite3 && chmod 600 /private/dir/clone.sqlite3
cargo run --release -p st3 --example retention_plan -- /private/dir/clone.sqlite3 2026-10-10
```

It opens and migrates the clone, so never point it at a live store or at the copy you keep. Two
builds that plan the same clone with the same rules report the same digests.

# Lanes

A lane is one ordered line of entries that a mission run works through front first. The Small
Talk merge train is a lane: each entry is a pull request, and the train updates, tests, and merges
the front one at a time.

This document explains the model, the declaration, the commands, how the order is stored and
replicated, and what st leaves to the run that owns the lane.

## Model

- A mission declares a lane by name. Each run of that mission owns one lane subject,
  `lane/RUN/NAME`, for as long as the run is active.
- An entry is a graph subject, for example `resource/github/acme/app/ci/pull-request/42`.
- Anyone with a person or agent identity can join an entry. A joined entry goes to the back.
  Joining an entry that is already in the lane changes nothing.
- An entry leaves the lane with an outcome: `completed` when the run finished its work for it,
  `removed` when it was dropped. A reason says why. An entry that left can join again; it goes to
  the back, and its old status and approval do not come back with it.
- A move places one entry at the top, at the bottom, or directly before or after another entry.
  Sending an entry to the back is a move to the bottom.
- The run that owns the lane records each entry's status: `waiting`, `held`, `ready`, or
  `running`, with a short detail and, optionally, the exact head it applies to. A newly joined
  entry is `waiting` until the run first checks it.
- A lane can name one `approver`. Only that person can approve an entry. The run decides which
  entries need an approval and holds them until one is recorded.

st stores and replays the lane and shows it. It does not decide when an entry is ready or what
working on it means. The run's own work does that, for example an `exec` runtime declared next to
the lane. A lane never starts, cancels, or reorders mission runs by itself.

## Declaration

```kdl
mission "example/app/merge-train" state="ready" {
  goal "Merge ready pull requests into main one at a time."

  lane "app" {
    entries "resource/github/acme/app/ci/pull-request/"
    approver "person/ada"
  }

  exec "driver" {
    host "local"
    workspace "/var/lib/merge-train"
    command "/usr/local/bin/merge-train drive"
    env { TRAIN_LANE "lane/${ST_MISSION_RUN}/app" }
    restart "always"
  }
}
```

`entries` is an optional subject prefix. When it is set, every entry must start with it, and the
commands accept the rest of the subject alone, so `42` means
`resource/github/acme/app/ci/pull-request/42`. `approver` is optional and must be a
`person/...` subject.

A lane is declared at mission level. Its name follows the rules for other run-owned declarations.
A revision can change `entries` and `approver`; the entries and their order stay, because they are
claims on the same lane subject. When the run ends, the lane is closed and no longer listed.

## Commands

```sh
st lanes ls
st lanes show app
st lanes join app 42 --reason "reviewed and green"
st lanes leave app 42 --reason "superseded by 43"
st lanes move app 42 --bottom --reason "main moved during the run"
st lanes approve app 42 --as person/ada
st lanes mark app 42 --state running --detail "testing 1f2e3d4 on st/ci" --head 1f2e3d4...
```

A lane argument is a full `lane/RUN/NAME` subject, the run of a mission that declares exactly one
lane, or a lane name that only one active lane uses. An entry argument is a full subject, or the
part after the lane's `entries` prefix; a leading `#` is dropped.

Every mutation records its actor. An agent inside a harness acts as its own seat. A person passes
`--as person/NAME`, or the command uses the `person` from the st configuration. `approve` refuses
any actor except the lane's approver.

`st lanes show` prints the entries in order with their status, and the recent joins, leaves,
moves, and approvals, newest first:

```text
LANE      lane/example/app/merge-train/1/app
RUN       mission-run/example/app/merge-train/1
ENTRIES   resource/github/acme/app/ci/pull-request/
APPROVER  person/ada
QUEUE     3
  1. 42  running  testing 1f2e3d4 on st/ci  joined by agent/example/worker 12m ago
  2. 44  ready    st/ci passed on 9a8b7c6  joined by person/ada 8m ago
  3. 45  held     waiting for person/ada to approve  joined by agent/example/fixer 2m ago
RECENT
  moved 43 to the bottom by agent/example/driver 15m ago: main moved during the run
```

`st missions show RUN` lists the lanes the run owns with the same entries, and `st missions tree`
lists every active lane under `LANES`. The client API serves the same view: the `lanes`
collection, `lane.get`, and the actions `lane.join`, `lane.leave`, `lane.move`, `lane.mark`,
and `lane.approve`.

## Storage and replication

Each change is one claim on the lane subject:

| Claim | Fields |
| --- | --- |
| `lane.joined` | `entry`, optional `reason` |
| `lane.left` | `entry`, `outcome` (`completed` or `removed`), optional `reason` |
| `lane.moved` | `entry`, `placement` (`top`, `bottom`, `before`, `after`), `anchor` for before and after, optional `reason` |
| `lane.marked` | `entry`, `state` (`waiting`, `held`, `ready`, `running`), optional `detail` and `head` |
| `lane.approved` | `entry`, optional `reason` |

A replica rebuilds the same lane from the same claims. It applies them in graph time. Claims with
the same time keep the store's replica-stable order: by writer, then in the order that writer
recorded them. A move, mark, or approval that names an entry which is not in the lane
at that point changes nothing, and so does a move whose anchor is not in the lane. The declaration
itself is the lane subject's `intent.desired` claim, written when the run starts, like the run's
observers and schedules.

## Tests

- `crates/st3/src/lane.rs` unit tests cover the replay: join order, rejoin, moves in every
  placement, and marks and approvals that belong to one membership.
- `crates/st3/tests/client_v0_cli.rs` declares a lane, lets the reconciler write it for a run, and
  drives it through the CLI: join by prefix, a harness joining as its own seat, show, move, mark,
  approve by the approver only, leave, rejoin, the lane in `missions show` and `missions tree`,
  and a lane that closes with its run.
- `crates/st3/tests/client_v0_contract.rs` checks the `lanes` collection, `lanes.get`, and the
  lane actions, including the approver check.

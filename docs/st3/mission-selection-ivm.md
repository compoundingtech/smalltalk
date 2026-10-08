# Prepared mission selection view

`store::mission_ivm` is an opt-in shadow operator over Smalltalk's admitted
`mission_runs` and `mission_definitions` projections. It does not register itself
in the default runtime or replace a production reader. Shared runtime installation
and the Store Publisher belong to the collection integration; the operator uses
`smallclaims::ivm::View`, local changes and its existing committed-key feed.

The view ID is `st3.mission-selection.v1`. Keys are public `mission/<id>` subjects.
Rows contain selected definition/latest-run headers, per-status run counts and
current membership. They do not contain complete mission cards or work items.

Source triggers retract/add old/new status counts and retain affected mission
keys. Flush pages maintain only those keys. Latest run selection uses the current
reader's created-time text DESC/run-ID DESC order, including ties. Selection uses
numeric updated-time DESC/mission-ID ASC and the existing continuation tuple.
The optional literal ID filter is applied before LIMIT. Public IDs are Unicode
lowercased with Rust during maintenance; the bounded needle is likewise lowercased,
preserving Unicode and mixed-case matching without SQLite's ASCII-only `lower`.
Filtered selection can visit the ordered candidate index until enough matches
are found, like the existing list matcher; its returned page is bounded. A
selected header/count batch uses one indexed query.

Current membership has a separate ordered partial index. Failed/cancelled runs
remain included at exactly the 24-hour boundary. An explicit clock page processes
only indexed expired membership keys, with a configurable bound up to 1024. A
read refuses overdue or unflushed source state, partial time maintenance and a
captured time older than maintained membership. It performs no installation,
flush, history fold or read-triggered repair. The next deadline is the first
indexed eligible grace expiry plus one millisecond.

Consumers must certify complete admitted/projected dependency coverage before
calling source flush. The primitive's frontier equality is necessary evidence,
not that certificate. Ordinary registration on populated sources cannot seed old
counts; populated installation requires a separately reviewed explicit adapter.
Canonical admission, repairs and run-tree ordering remain owned by the existing
production source projection. Unsupported source mutations fence output instead
of promoting old state.

The local fixtures use real Store publication, run actions and replication, with
the old selection SQL and independently grouped source counts as test oracles.
Controls cover tie seeks, filtering before slots, duplicate/permuted replication,
old/new membership, retirement, inclusive time boundaries, rollback, reopen,
source mutation fencing, partial clock pages and unchanged-output mutation
witnesses. Query-plan controls require ordered/deadline source indexes and reject
an intermediate sort. These are correctness controls, not writer-cost or deployed
CPU measurements.

Run the cohort with the standard Cargo wrapper and configured target runner:

```sh
cargo test --locked -p st3 --lib mission_ivm -j 6 -- --test-threads=4
```

Activation still requires complete commit/source-availability hooks, explicit
populated installation/checkpoint/restore proof, and full card dependencies:
bounded run previews, effective steps, generations/ancestors, summaries/queue,
faults, attention, ownership/authority and fresh clock inputs. Work ranking and
presentation require their own operator. The selection-only ID must not be used
to label those broader relations ready.

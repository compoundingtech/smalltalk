# Explicit asynchronous claims-only views

`ViewRuntime::asynchronous(views, limits)` is opt-in for a fresh store or a compatible
asynchronous file. No existing production runtime, route or reader changes. A nonempty
unregistered or synchronous store is refused before projection schema work. Ordinary open
resumes compatible metadata only; it neither folds history nor starts a worker. Adding,
removing or changing registered definitions requires a separately reviewed explicit
installation. Keep exclusive runtime ownership of the file; a different Plain handle can
bypass the reference runtime's checkpoint and admission policy.

Raw admission commits the claim and an indexed delta entry together. A queue quota failure
preserves the admitted claim, marks the source unavailable, stops catch-up and requires
explicit recovery. Further admitted claims are preserved without pretending their missing
deltas were processed. Outer rollback/storage failures commit neither claim nor delta.
The reference runtime has Plain-like admission, not production signature/authority closure.
Adapters must preserve their actual admission policies and complete source capture.

`asynchronous::step` performs one page and returns `PageReport`. Every page verifies the
entire bounded registered name/fingerprint/epoch set before maintenance; a caller cannot
skip a view or change its operator by supplying another runtime. Input row and encoded-byte
bounds apply before fetching/decoding claim bodies. The queue has at most 4,096 entries and
16 MiB of claim-field payload; a page has at most 128 entries and 1 MiB. Defaults are 8 rows
and 256 KiB per page, with 4,096 rows/16 MiB queued. SQLite/index/page overhead is additional.
A first input exceeding the page byte cap stops rather than skipping it. Registered views
number at most 256, and their combined declared contribution/dependency bounds are at most
256. Operator-owned SQL, allocations, output size and CPU require their own bounded proof;
these declaration limits do not preempt an arbitrary Rust callback or guarantee a hard
writer deadline. Use smaller pages when needed.

The canonical tuple remains unchanged. Indexed record MIN is used when present; fallback
COUNT runs only after an indexed prefix probe proves the earlier same-batch rows fit the
configured legacy rank bound (default 32, maximum 128). An oversized legacy batch fences
its subscribed views while healthy views continue. Accepted repairs conservatively fence
affected views for explicit bounded recovery; they still preserve source admission.
Local/deadline source operators are refused by this constructor. Ordered mission trees,
external transcripts, numeric snapshot adapters, authority closure, populated-store async
installation, checkpoint/retention and mixed-runtime fleet adoption remain consumer work.
No read, repair or startup fallback rebuild is supplied.

## Prefix and availability

The durable queue contains every admitted input above the committed source prefix. A page
processes its prefix ordered by local `store_index` and atomically publishes output, contributions, dependencies,
generations, per-view completeness evidence, source cut and queue reclamation. Storage
errors roll back the page and propagate. Operator errors roll back that operator's
savepoint and persist an unavailable view, without rejecting valid source admission.
Queue order is not canonical claim order: a late replicated claim can have an earlier
canonical rank. `Views::change` applies the canonical rank key just as synchronous
maintenance does; the shuffled-replication oracle controls check this schedule.

`applied_prefix(connection, views, view, epoch)` returns the certified source prefix only
for a compatible view whose completeness certificate has never been broken. It returns
None for missing/incompatible/fenced evidence. A raw ready boolean cannot restore that
certificate. Maximum observed claim index and the diagnostic `through` field are not
completeness proofs. Unrelated inputs advance the common certified prefix without writing
every view or changing semantic generations. Readiness conservatively requires catch-up
through all currently admitted claims plus complete view evidence. This first adapter uses
a common processing prefix, rather than independent queues for each view.

Source deletion/remap, insertion behind a certified prefix, queue deletion/replacement, prefix jumps over captured input and
missing/replaced completeness metadata remain explicitly unavailable. These controls do
not certify arbitrary external SQL mutation or restoration. Event database identity and
source epoch still need explicit restore handling. The checkpoint refusal remains.

## Scheduling and reads

Install `events` explicitly and retain one `Publisher` for the Store. Subscribe before
starting `Worker::start`; it drains one blocking page, yields between pages so ordinary
writes can interleave, and awaits committed notices when idle. It retains no read snapshot
between pages. Progress is available through a watch receiver. `stop().await` finishes the
owned page and leaves committed progress resumable; dropping signals stop. Database errors,
quota failures and publisher closure terminate rather than retrying forever. The receiver
must belong to this Store. No startup/GET automatically schedules this work.
Receiver/Store matching is a caller precondition; `Worker::start` does not verify it.
Share the one Store-owned Publisher across worker subscriptions, receipt waits and sockets.

There is no implemented recovery API for quota overflow, missing capture, source mutation
or an input larger than `page_bytes`. One input over the default 256 KiB page cap stops
catch-up without skipping it. More than 4,096 queued rows, including one large replication
batch arriving before catch-up, can permanently disable this async source. Restarting the
worker or reopening the Store does not repair either condition. References to explicit
recovery name a required future installation/recovery contract, not a supplied procedure;
production adoption requires that contract or admission/startup bounds that prevent these
conditions. Directly resetting SQL metadata cannot certify missing work.

An operator panic rolls back its whole page and ends the worker with an error, available
through `stop().await` or closure of the progress channel. It does not persist a new
panic-specific availability reason; the queued input remains unprocessed and current reads
remain unavailable. A deterministic poison input can fail on every restart. `stop().await`
has no built-in timeout and cannot preempt an operator that never returns. Dropping the
worker signals stop but discards its final error; an owner needing diagnostics must retain
progress and join via `stop`. Panic fencing and bounded shutdown need an explicit production
adapter contract before adoption.

Use `after_write::write` and `after_write::wait` for read-your-writes. Receipt mapping,
source prefix, view readiness, output and authorization are captured in one short snapshot;
waiting releases it. The output callback must enforce authority/owner/incarnation and
complete dependencies; later effects need a fresh fence. Async page commits use the same
key invalidations and independent availability stream. Availability becoming Ready requires
an authorized bounded-window refresh even when that client's key page is empty. Those
invalidations do not witness every historical transition.
Readiness requires the global admitted prefix, so sustained writes can keep receipt waits
pending even after their own input was processed. This API promises no bounded wait under
that load. A permanently unavailable async source is represented as `SourcePending` with
an error in availability; `after_write::wait` keeps waiting until deadline/cancellation
rather than returning a separate unavailable outcome. Consumers that need fail-fast errors
must inspect availability themselves in an authoritative snapshot.

## Consumer interfaces and readiness examples

| Consumer | Interface | Required same-snapshot evidence |
| --- | --- | --- |
| Agent rows | `Views::readiness`, then the bounded keyed output query | Complete source prefix, compatible definition/epoch, current actor/owner/incarnation and all row dependencies |
| Mission and attention rows | The same readiness check plus the consumer's captured-time/local-source certificate | Complete claim and captured-time coverage, current generation/blocks/deadlines and all dependencies; the reference async constructor refuses these local-source definitions |
| Row delta bridge | One Store-owned `Publisher`, `events::capture`, `events::keys` and `events::availability` | Subscribe before snapshot, authorized bounded window, current readiness and retained cursor/identity; an availability change can require a refresh even without changed keys |
| Write response | `after_write::write`, then `after_write::wait` with `Target::Local` or explicit `Target::Replicated` | Receipt mapped in this database, complete source prefix, ready view and authorization inside the output callback |

For example, with admitted input 12 and certified prefix 10, a page can have committed
some output for input 10 while the current view remains `SourcePending`. A receipt for
input 10 also remains pending: neither that partial row nor its largest applied index
permits a current read. Once all captured inputs through 12 commit, an intact compatible
view can become `Ready`; a dependency/operator fence still prevents readiness at that
same prefix. A raw `ready=1` update cannot restore a broken completeness certificate.

An unknown-kind input advances the certified common prefix without changing output
generation. An accepted unsupported repair fences affected views while retaining raw
admission. Source remap or missing capture stops certification and requires explicit
recovery. None of those states authorizes serving a cached or recomputed current answer.

Production consumers keep their existing admission runtime. `ViewRuntime::asynchronous`
is a reference claims-only runtime, not a wrapper around another runtime; replacing a
production runtime with it would lose that runtime's admission policy. A populated-store
or captured-time adapter must supply its own explicit installation and complete coverage
proof before activation. The worker never installs such an adapter implicitly.

`ivm_async_lifecycle` tests stop with an interleaved writer during an owned page, resumable
pending input, worker drop during a page and while idle, Publisher closure while idle,
lagged notices and repeated poison-input panic rollback. Stop completes the current page before
returning; the next queued input stays durable and unavailable until a later explicit page.

```sh
cargo run --locked -p smallclaims --example ivm_asynchronous
cargo test --locked -p smallclaims --test ivm_async
cargo test --locked -p smallclaims --features test-support --test ivm_async \
  unchanged_answer_writer_cost_and_retained_candidate_growth -- --ignored --exact --nocapture
```

The isolated cost test must run alone because SQLite counters are process-wide. It records
raw writer time through COMMIT, commit observers and guard return, separately from writer
wait; page reports use the same inclusive interval. It compares 100/1,000 same-answer
mailbox histories, asserts unchanged semantic generation, reports retained contribution
rows/payload bytes, and separately measures genuine 1/8/32-output pages. Candidate state
still grows with eligible retained history; no generic garbage collection is introduced.
These small in-memory fixtures are not a production 2 GiB or run-tree p99 acceptance test.

## Small local fixture results

The first isolated probe used 20 measured samples per shape, after its history setup. With only
20 samples, nearest-rank p99 is the maximum; these are observations, not timing gates.
Times include the lent writer's commit/observer/return interval, and can include scheduling.

| Same-answer history | Synchronous hold max | Async raw hold max | Async page hold max | Contributions / payload bytes |
| --- | ---: | ---: | ---: | ---: |
| 100 | 1.116 ms | 2.259 ms | 2.670 ms | 122 / 50,602 |
| 1,000 | 1.386 ms | 0.784 ms | 1.378 ms | 1,022 / 425,025 |

At both sizes synchronous maintenance used 29 statements/1,240 VM steps; async admission
used 15/1,199 and its one-input page used 44/2,918. The extra queue/prefix work is real;
this probe does not establish a latency improvement. Semantic answers/generations stayed
unchanged, and every measured page reclaimed its delta. Contribution payload grows linearly
here and excludes SQLite/index overhead. Rank/provenance and retained canonical history
also require consumer retention policies.

Genuine 1/8/32-output pages had maxima of 1.900/6.793/28.550 ms, respectively; statements
were 52/227/827 and VM ranges 3,195–3,197 / 8,200–8,304 / 25,360–25,776. These are small
in-memory fixtures, not the requested production run-tree or copied 2 GiB acceptance.
The earlier unchanged-answer mixed-rank design's history-linear writer failure remains
rejection evidence; oversized fallback ranks are fenced here rather than changing order
or claiming that failed case was solved for unrestricted historical batches.

A second probe on the committed successor observed materially higher holds: same-answer
100/1,000 async admission maxima 12.666/45.565 ms and page maxima 3.607/44.817 ms;
synchronous maxima were 4.243/2.678 ms. Genuine 1/8/32-output page maxima were
10.867/23.603/115.822 ms. The 32-output shape exceeds the 50 ms target. Both runs are
retained; no scheduling/CPU cause is inferred. The default is therefore eight input rows;
32-row cost shapes explicitly request that larger page. This reduces per-page work, not
wall-clock scheduling delay, and does not establish a production timing guarantee.

The capture format is `smallclaims.ivm.async.v2`. Earlier prototype capture formats are
refused before projection schema work; unchanged IF-NOT-EXISTS triggers from an older
format cannot silently count as current coverage. No automatic format upgrade is supplied.

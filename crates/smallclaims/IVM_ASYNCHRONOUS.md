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
processes its ordered prefix and atomically publishes output, contributions, dependencies,
generations, per-view completeness evidence, source cut and queue reclamation. Storage
errors roll back the page and propagate. Operator errors roll back that operator's
savepoint and persist an unavailable view, without rejecting valid source admission.

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

Use `after_write::write` and `after_write::wait` for read-your-writes. Receipt mapping,
source prefix, view readiness, output and authorization are captured in one short snapshot;
waiting releases it. The output callback must enforce authority/owner/incarnation and
complete dependencies; later effects need a fresh fence. Async page commits use the same
key invalidations and independent availability stream. Availability becoming Ready requires
an authorized bounded-window refresh even when that client's key page is empty. Those
invalidations do not witness every historical transition.

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

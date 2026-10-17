# st observability

This reference describes the current daemon, CLI, replication worker and hook
telemetry in the `crates/st3` implementation. The shared client-class vocabulary
and bounded attributes are used by production code.

## Requirements

O11Y-R10–R18 remain the accepted constraints. They include coverage targets that
are not all implemented; the mechanism and integration boundaries below state the
current coverage. Budget limits are requirements, not claims of measured compliance.

- **O11Y-R10 Process signals:** The st3 daemon, replication worker, and CLI use the OpenTelemetry
  SDK to produce traces, metrics, and logs. Their `service.name` values are `st-daemon`,
  `st-replication-worker`, and `st-cli`. Hooks send signals through the daemon and never export
  directly; hook spans and invocation metrics emitted by the observations exporter use
  `st-hook`. Harness-timer statusline commands remain exempt from telemetry initialization.
- **O11Y-R11 No-op when unset:** When `OTEL_EXPORTER_OTLP_ENDPOINT` is unset, st3 builds no
  exporter or SDK provider, makes no telemetry network calls, and incurs no measurable telemetry
  overhead. Exporter configuration uses standard `OTEL_*` variables.
- **O11Y-R12 Reactor isolation:** Export work must not run on the daemon's request reactor.
- **O11Y-R13 Signal version:** Every st3 signal carries `service.version` from the machine build
  identity, including signals from the observations exporter.
- **O11Y-R14 Bounded metrics and attributes:** st3 exposes a bounded rate, error, duration, and
  saturation metric set with at most 2,000 active series per daemon. Agent, session, message,
  terminal, attachment, and lease ids are forbidden in metric labels and `span.label`.
  Capabilities and their hashes are forbidden in every signal.
- **O11Y-R15 Trace propagation:** W3C `traceparent` and `tracestate` propagate end to end across
  st3 HTTP and WebSocket upgrades, Rust, TypeScript, and Swift clients, and the signed peer
  protocol. Peer trace context is covered by the peer signature.
- **O11Y-R16 Sampling:** st3 exports every span when traces are enabled and performs no
  in-process sampling. The local collector applies the fleet sampling policy: keep traces
  with an error, a local root longer than 1 second, a sampled caller, and a deterministic
  1% of the rest. Sampling never affects metrics.
- **O11Y-R17 Bounded shutdown:** Telemetry flush and shutdown take at most 250 milliseconds
  for a CLI process and at most 5 seconds for a daemon or replication worker, including when
  the collector is unreachable.
- **O11Y-R18 Overhead budget:** With export enabled at fleet load (4–5 requests per second and
  0.7 reconcile passes per second), telemetry adds at most 2% daemon CPU, 5% p99 request
  latency, and 32 MiB RSS. Sampled export traffic is at most 50 KB per second per host.

## Pipeline

```text
process tracing ── AlwaysOn ── batch span processor ───────────┐
metric instruments ── periodic reader ────────────────────────┼── SDK threads ── OTLP/HTTP JSON
tracing events ── correlated log bridge ── batch processor ───┘
hook signals ── daemon observations exporter ────────────────────────────────── OTLP/HTTP JSON
```

## Pipeline and identity

`crates/st3-telemetry/src/otel.rs` owns SDK initialization, resource construction, and shutdown.
The tracer provider uses `Sampler::AlwaysOn` and a plain SDK `BatchSpanProcessor` to export
every span, including spans with an unsampled remote parent. There is no in-process sampler
or span buffer beyond the SDK batch queue. The manifest pins OpenTelemetry API, SDK
and OTLP crates at 0.30, with
`tracing-opentelemetry` 0.31. OTLP uses HTTP JSON and the blocking reqwest client.
The batch queue is bounded below the SDK defaults because O11Y-R18 also caps daemon RSS:
`max_queue_size` 256 and `max_export_batch_size` 256, with the SDK's default
`scheduled_delay` (5 s, `OTEL_BSP_SCHEDULE_DELAY`). This queue bound does not
establish the process's RSS or the overhead budget. The `BatchConfigBuilder` setters override the
environment, so `span_batch_config` re-applies `OTEL_BSP_MAX_QUEUE_SIZE` and
`OTEL_BSP_MAX_EXPORT_BATCH_SIZE` explicitly — those variables keep working. A full queue
drops spans; the SDK counts the drops and reports the first drop plus the shutdown total
through its internal `otel_warn` diagnostics (`BatchSpanProcessor.SpanDroppingStarted`),
which the stderr layer prints. There is no exported drop counter.
Trace, metric, and log exporters use SDK-owned threads; none export on the daemon's
`new_current_thread` request reactor. The log bridge uses
`experimental_use_tracing_span_context` to attach the active trace and span ids, and
exports INFO-and-above only: per-request framework events are DEBUG and stay on stderr,
so a healthy daemon exports no log stream, while WARN/ERROR diagnostics still export.

With no `OTEL_EXPORTER_OTLP_ENDPOINT`, initialization builds no telemetry subscriber,
SDK providers, exporters, or threads. An atomic enabled gate returns before span construction.
Local stderr diagnostics remain available. Standard `OTEL_*` environment variables configure
export. `OTEL_SDK_DISABLED=true` (case-insensitive) selects this disabled path for every st3
unit. `ST3_CLI_OTEL=off` selects it for the CLI only.

| Process unit | `service.name` | Shutdown budget |
| --- | --- | --- |
| `st up` daemon | `st-daemon` | 5 s |
| `peer::run_worker` replication worker | `st-replication-worker` | 5 s |
| One-shot CLI | `st-cli` | 50 ms when export is enabled |
| Driver hook | `st-hook` via daemon observations exporter; no direct SDK export | No collector flush in the hook |
| `driver claude-statusline` | None; `telemetry::local_only()` | No pipeline |

`crates/st3-telemetry/src/telemetry.rs` remains the hook path. Hooks hand signals to the daemon and
never contact a collector. The observations exporter emits hook spans and hook invocation
metrics as `st-hook`; observation logs and usage metrics retain `st-daemon`. Statusline calls remain local-only and do not initialize an SDK pipeline.

The shared resource contains `service.name`, `service.version` from
`st_drivers::version::machine_version()`, a random per-process `service.instance.id`,
`host.name`, and `st3.node`. The observations exporter in `crates/st3-telemetry/src/otlp.rs` uses this
resource builder, including the version: hook spans and invocation metrics are `st-hook`,
observation logs and usage metrics are `st-daemon`. The bare `st` service name is retired.
The platform edge supplies fleet-owned attributes.
Flush and shutdown share one process-unit deadline across all three providers; an unreachable
collector cannot extend it.

Agent shells export the endpoint globally, and agent loops call the CLI thousands of times
per hour. A hung collector must not delay each call. After the CLI root ends, when export is
enabled, the CLI always waits at most 50 ms for a detached helper to flush and shut down all
providers. This is one hard deadline, not a separate budget per provider.
The daemon and replication worker retain their 5 s deadline.

A CLI flush timeout or an export error returned by provider `force_flush` or `shutdown`
records the failure time in `otel-cli-backoff`. The file is in `$XDG_RUNTIME_DIR/st3/` when
`XDG_RUNTIME_DIR` is set; otherwise it is in `$XDG_STATE_HOME/st3/`, with
`~/.local/state/st3/` as the fallback when `XDG_STATE_HOME` is unset. CLI initialization
reads this small file once, without locks or waits. If the current time is before the failure
time plus 300 s, it selects the disabled path before creating the pipeline. Missing or corrupt
files do not disable telemetry. Writers use a temporary file and atomic rename; readers
tolerate concurrent writers. This negative cache does not affect the daemon or replication
worker.

## Collector sampling policy

The local collector applies tail sampling keyed by trace id (O11Y-R16), keeping a trace if
any span has status `ERROR`, a local root lasts more than 1 second, or the root came from a
sampled caller. It keeps a deterministic trace-id ratio of 1% of the remaining traces.

The sampled-caller signal is `st.parent.sampled`: because st3 exports every span with
AlwaysOn, every exported span carries the sampled flag, and the collector cannot recover the
caller's decision from trace flags. Server spans set `st.parent.sampled=true` only when
the remote parent is sampled and its tracestate has no `st` key. st's own clients mark
collector-owned sampling with `st=c`; their AlwaysOn flag must not force the collector to
keep every trace. Unsampled or st-marked parents record false; a missing parent records no
attribute. The decision wait must be long enough for the daemon's SDK batch delay and
delivery of the completed root and its spans. RED metrics are exported independently and
are never sampled.

## Server request spans

`response_envelope` constructs one local root span per request only when trace export is
enabled. Its name is `"{METHOD} {route}"`, where `route` is the matched route template or
`/unmatched`, never the raw path. The request is a single span, with phases recorded as numeric root attributes
instead of per-phase child spans.

| Attribute | Meaning |
| --- | --- |
| `st.admission.queue_ms` | Wait for the admission `spawn_blocking` slot (client requests) |
| `st.admission.authenticate_ms` | `authenticate` duration (client requests) |
| `st.admission.snapshot_ms` | Cursor snapshot duration (client requests) |
| `st.handler.queue_ms` | Wait for the handler `spawn_blocking` slot (non-health routes) |
| `st.handler.duration_ms` | Handler duration, including `/v1/health` |
| `st.request.id` | Generated before the handler; identical to the envelope's `request_id` |
| `st.roster.mode` | `hit`, `incremental`, or `cold` for the roster projection read |
| `st.roster.cards` | Number of cards in that roster projection |
| `st.page.rows` | Number of rows in the returned page, after truncation |
| `st.page.bytes` | Serialized response envelope bytes for a returned page |

The stage durations in this table and card/page counts are integers; `st.request.id` and
`st.roster.mode` are strings. Roster phase and wait durations below use doubles.
Attributes are absent when the corresponding stage did not run.
The collector's slow-request and error policies key on the server span's duration and status.

The root attributes are `http.request.method`, `http.route`, `http.response.status_code`,
`st3.client.class`, and `span.label` equal to the route. A 5xx response or a handler error
sets status `ERROR`; a 4xx response alone does not. WebSocket routes end the server span at
the 101 response, not when the socket closes.
Upgrade handlers accept absent telemetry context: an unset exporter never changes
handshake authorization or first-frame delivery.

The server extracts W3C `traceparent` and `tracestate` from HTTP request and WebSocket
upgrade headers and uses the extracted context as the parent. The sampled-caller signal
follows the collector sampling policy above; the process exports every span.

## Subscription first frames and slow-path roots

```text
HTTP upgrade SERVER span (ends at 101)
    <link> st.subscription.first_frame INTERNAL root (subscribe -> first frame sent)
HTTP receive SERVER span
    <link> st.replication.projection INTERNAL root (new or deferred data only)
st.roster.rebuild INTERNAL root (cold build only; optional admission-holder link)
```

All four client stream socket tasks carry the upgrade's `SpanContext`, not a live server
span. The context is captured before handler dispatch because the handler constructs the
`on_upgrade` closure before returning 101; it is also cloned into conversation followers
and terminal pumps. W3C context on the upgrade therefore survives task boundaries without
extending the request span's lifetime.

Each accepted subscribe constructs one `st.subscription.first_frame` root with an empty
parent context and a link to the valid upgrade span context. It ends after the first
frame is sent. Later rereads, resyncs, and changes construct no spans; there are no
per-frame child spans. Construction is gated by the trace-export `AtomicBool`, before
labels, collections, or counts are inspected (O11Y-R02, O11Y-R18).

| First-frame attribute | Type / vocabulary |
| --- | --- |
| `st.collection` | String: `missions`, `attention`, `agents`, `work`, `glasses`, `arrangements`, `conversation`, `terminal` |
| `st.subscription.id` | String: protocol subscription identity, span-only |
| `st.projection.hit`, `st.projection.cold`, `st.projection.incremental`, `st.projection.shared` | Boolean: projection path used by the initial read |
| `st.page.rows` | Integer: initial frame rows after truncation |
| `st.page.bytes` | Integer: serialized initial frame bytes |
| `span.label` | Bounded collection/projection description, never an identity or path |

`st.roster.rebuild` surrounds only cold roster builds, including cold historical pages.
It is detached, with no parent, `span.label=cold`, `st.roster.mode=cold`, and
integer `st.roster.cards`. Cache hits and incremental refolds create no rebuild roots.
It has a link only when its admission waited behind a holder with a valid existing span.
A root started under roster admission ends when that admission is released.
Its duration runs from cold-build start to release; `st.roster.admission.hold_ms`
separately covers the full acquisition-to-release interval.
Only the physical worker owning that admission may bind a rebuild to it. A concurrent
internal reader without admission, including the startup diagnostic report, ends its
own root at build completion and records no invented admission identity.
Roster and page stage attributes on requests remain on the SERVER span, not children.

`st.replication.projection` surrounds the post-receive projection attempt only when
new data arrived or previously deferred data remains. It uses an empty parent context,
links to the receive SERVER span, and records `span.label=receive`,
string `st.replication.peer`, integer `st.replication.moved_envelopes`, and
`st.replication.outcome` equal to
`projected`, `unchanged`, `deferred`, or `error`. A failed projection has ERROR status.
Other projection callers do not emit this receive-specific root; empty heartbeats do
not emit it either.

All identities and numeric stage attributes above are span-only, never metric labels.
The frame-level trace field, writer-wait instrumentation, WAL checkpoints, and reconcile
roots are separate instrumentation surfaces, not part of these stage roots.

### Roster attribution

Roster latency is attributed by attributes on spans that already exist: the request
SERVER span or `st.subscription.first_frame` root that performed the read, and the
detached `st.roster.rebuild` root when the read rebuilt cold. No admission, phase,
worker, reader, or per-frame span is constructed. Every recorder returns before work when
trace export is disabled (O11Y-R02, O11Y-R18). With export enabled, attribute construction
and phase clocks additionally require a recording destination. Only the preparatory
warm worker explicitly collects attributes before its cold publication span exists.
Before a cold root exists, queue and reader waits occupy fixed scalar slots, not an
allocated attribute map. Values recorded while a rebuild runs are written to the current
span and copied to the active rebuild root.

```text
GET /v1/client/agents SERVER / first_frame     st.roster.rebuild INTERNAL root (cold only)
  st.roster.path, *_cut, fresh_requested          st.roster.invalidation, changed/covered/refolded
  st.roster.invalidation (incremental reasons)    st.roster.admission.id, holder_class
  st.roster.admission.* (own acquisition)         st.roster.phase.<sync phase>.*
  st.roster.worker_queue_ms, reader_acquire_ms    st.roster.worker_queue_ms, reader_acquire_ms
  <link> blocking holder (contended waiter only)
```

| Attribute | Type | Vocabulary / meaning |
| --- | --- | --- |
| `st.roster.path` | string enum | `published_complete`, `published_head`, `exact_warm`, `exact_cold`, `filtered_full`, `not_ready`; any status-filtered read is `filtered_full` |
| `st.roster.fresh_requested` | boolean | Whether the HTTP agents query asks for legacy `fresh=true` or a named `min_cut`, including exact reads, published first pages, continuations and `not_ready`; WebSocket reads and background rebuild roots report `false` because they have no freshness query |
| `st.roster.fresh_wait` | string enum | Exit of the fresh published-first-page wait: `published`, `unchanged`, `refresh_failed`, `disconnected`, `timeout`, `read_failed`; absent when no wait ran |
| `st.roster.fresh_wait_cut` | integer | Actual wait target read when the wait starts, distinct from the earlier request cut and the publication served; absent if that index read failed |
| `st.roster.fresh_proof` | string enum | Proof for the exact complete publication served: `not_checked` for other wait exits, `verified`, `refused` (including head-only, not-ready or evicted publication), `read_failed` |
| `st.roster.fresh_unchanged_verified` | boolean | True exactly when `fresh_proof=verified`; only an `unchanged` wait exit can receive verification |
| `st.roster.requested_cut` | integer | Named `min_cut`, if supplied; otherwise store index of the read's snapshot |
| `st.roster.publication_wait_ms` | double | Milliseconds spent in the explicit `min_cut` publication barrier |
| `st.roster.publication_cut` | integer | Store index of the publication served; absent for `not_ready` |
| `st.roster.worker_queue_ms` | double | Cumulative submission-to-start time of the read's blocking workers, excluding reader acquisition and execution |
| `st.roster.reader_acquire_ms` | double | Cumulative SQLite reader acquisition, connection opening, `BEGIN`, and first index read |
| `st.roster.admission.id` | integer | Process-local sequence number of this admission acquisition |
| `st.roster.admission.holder_id` | integer | Acquisition id that held admission while this waiter queued |
| `st.roster.admission.holder_age_ms` | double | Blocking holder's age at wait start, measured from its monotonic acquisition instant |
| `st.roster.admission.waiter_class` | string enum | Class of this acquisition: `http`, `ws`, `refresher`, `other` |
| `st.roster.admission.holder_class` | string enum | Blocking holder's class: `http`, `ws`, `refresher`, `other`, or `unknown` when admission is reserved but its holder metadata is unpublished; on an uncontended rebuild root, class of its own admission |
| `st.roster.admission.wait_ms` | double | Enqueue to acquire |
| `st.roster.admission.hold_ms` | double | Acquire to release |
| `st.roster.invalidation` | string enum | `cold`, `unknown_kind`, `structural_claim`, `missing_message`, `membership`, `local_frontier`, `queue_deadline`, `coverage_gap`, `safe_delta`, `historical_snapshot` |
| `st.roster.changed`, `st.roster.covered`, `st.roster.refolded` | integer | Changed agents, resulting subject coverage (card count for a complete roster), and cards refolded |
| `st.roster.phase.<phase>.wall_ms`, `st.roster.phase.<phase>.cpu_ms` | double | Wall and calling-thread CPU time; `<phase>` is `membership`, `status_fold`, `card_presentation`, `subagent_leases`, `queue_selection`, or `serialization` |

Phases are synchronous sections measured on the thread that runs them, so `cpu_ms` is
that section's own thread CPU rather than a share of an outer task. Durations keep
sub-millisecond precision as doubles. Repeated phases within one read accumulate.
`subagent_leases` measures the live lease query inside `card_presentation`; it is a
component of that enclosing phase, not an additional duration to sum with it.
Worker queue and reader acquisition totals survive a WS probe and its admitted retry.
The first publication includes its preparatory warm worker's measurements, moved into
the cold root without creating another span. Those sections can precede root start.
Each CPU sample surrounds only a synchronous section on one thread; both ending clocks
are sampled before recording attributes. The handoff's async join contributes no CPU
time. Admission, worker queue, and reader acquisition are wall-only waits, not CPU phases.

The rebuild root carries `cold`, `unknown_kind`, `structural_claim`, `missing_message`,
or `historical_snapshot`. A read that refolds a prior projection records `membership`,
`local_frontier`, `queue_deadline`, `coverage_gap`, or `safe_delta` on its current span
without constructing a root.

Every acquisition begins as a waiter: it records its own `id`, `waiter_class`, `wait_ms`,
and, on release, `hold_ms`. The first contended mutex poll and blocking-holder snapshot
are synchronized with holder publication and release under one metadata lock. The
snapshot is frozen across later polls. A known blocker contributes `holder_id`,
`holder_class`, and `holder_age_ms`; an unpublished reserved holder contributes only
`holder_class=unknown`, never a guessed id, age, event, or link. A known blocker carries one
link to its span context only when that existing context is valid, with the
`st.roster.admission.holder_id` link attribute. At acquisition, waits of at least 1 ms
emit `st.roster.admission.wait` on the current waiter span when one exists, with holder
id, both classes, and `wait_ms`. `holder_id` is separate from the waiter's own `id`
because the waiter becomes the next holder. A holder without a span of its own takes
the context of a cold rebuild
root it starts, so waiters can link to that root. Incremental refresher refolds have no
existing span: they are attributed by class, admission id, wait duration, and holder age,
not by a link. No root is added for those refolds. An admitted cold root records its own
admission `id` and class, and retains any blocker link when it was itself a waiter.
The aggregate `roster/admission-wait` accounting is unchanged.
Nonblocking try-admission uses the same holder registration and release contract, with
zero `wait_ms` on success and no attributes on a miss. Collection readers retain their
release-before-queue, try-before-admit gate ordering, so roster attribution never causes
them to retain reader capacity or unrelated gates while queuing. RosterStage performance
metrics remain alongside span attribution; admission wait accounting and the refresher's
admission metric reuse the guard's single monotonic wait measurement.

Privacy contract: every value above is an enum, a boolean, a process-local sequence
number, a store index, a count, or a duration. Subject ids, agent names, paths,
selectors, filter values, and claim bodies are never recorded. All roster attribution
is span-only and never a metric label or `span.label`.


## Metric naming and cardinality

The repository-local st3 instrument namespace uses lowercase dot-separated names:
daemon work and storage instruments use `st.`, while HTTP and database client
instruments use the OpenTelemetry `http.server` and `db.client` namespaces.
Duration instruments use seconds. Depth, size, and age gauges describe saturation.
Examples are `http.server.request.duration`, `st.db.writer.queue.duration`, and `st.fifo.depth`.
Do not append Prometheus `_total` or `_seconds` suffixes to these OTLP instrument names.

Label vocabularies are closed enums or bounded fleet membership. Unknown user-provided
values map to `other`; routes are matched templates, not raw paths.

| Label axis | Bound or vocabulary |
| --- | --- |
| `st3.client.class` | The 12 values in the client class table below |
| HTTP route, method, status class | Registered templates, methods, and status classes |
| `claim_family` | Top-level registered kind segment, else `other` |
| Reconcile `task` | `pass`, `deadline` |
| Wake `cause`, FIFO `queue`, startup `phase`, replication `result` | Closed registries |
| Replication `peer` | Fleet node membership |
| Allocator heap `state` | `allocated`, `free`, `mmapped`, `arena`, `releasable` |

`ClientClass::as_str` defines the closed `st3.client.class` vocabulary. Classification uses
the trimmed `x-st3-client` header, limited to 120 characters, with case-insensitive matching.
The rules run in this order: `omp-channel`, `replication-worker`, `hook`, `driver`,
`peer`/`fabric`/`relay`, `stui`, `smalltalk-`, `fractal`, `web`/`browser`, then CLI prefixes.
The class is observational only; it does not grant identity or authority.

| `st3.client.class` | Header shape |
| --- | --- |
| `cli` | `st`, `st <machine_version>`, `st <subcommand>`, or `st3 <subcommand>` such as `st3 doctor` |
| `stui` | Prefix `stui`, such as `stui <version>` |
| `fractal` | Prefix `fractal` |
| `web` | Contains `web` or `browser` |
| `app` | Prefix `smalltalk-`, including `smalltalk-ios <version> (<build>)`, `smalltalk-ide <version>`, and `smalltalk-example-tui <version>` |
| `replication-worker` | Contains `replication-worker`, such as `st3 replication-worker` |
| `omp-channel` | Contains `omp-channel`, such as `st3 driver omp-channel` or `agent/<seat> · st3 driver omp-channel` |
| `driver` | Contains `driver`, such as `st3 driver omp`, `st3 driver codex`, or `agent/<seat> · st3 driver <driver>` |
| `hook` | Contains `hook`, such as `st3 driver-hook` |
| `peer` | Contains `peer`, `fabric`, or `relay` |
| `other` | Any other present value; `curl` maps here on purpose, not to `cli` |
| `unknown` | Header absent or blank after trimming |

`http.server.request.duration` records request duration in seconds at the same point as
the in-memory meter, independent of trace sampling. Its labels are `http.request.method`,
`http.route`, `st3.client.class`, and `http.response.status_class`. An SDK cardinality limit
caps this instrument at 1,500 series; excess combinations go to the
`otel.metric.overflow=true` series. This runtime cap enforces O11Y-R14's 2,000 active-series
budget rather than the theoretical label product.

Duration buckets are
`0.001`, `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`,
`30`, and `60` seconds.

## Storage telemetry (O11Y-R14, O11Y-R18)

| Instrument | Type | Unit | Additional attributes / meaning |
| --- | --- | --- | --- |
| `db.client.operation.duration` | Histogram | `s` | `db.operation.name=write.batched` or `write.lend`; writer enqueue through ACK |
| `st.db.writer.queue.duration` | Histogram | `s` | `st.db.writer.origin`; enqueue through writer dequeue |
| `st.db.writer.batch.size` | Histogram | `{job}` | —; jobs in a writer batch |
| `st.db.writer.commit.duration` | Histogram | `s` | —; writer commit duration |
| `st.db.writer.batch.duration` | Histogram | `s` | —; whole writer batch duration |
| `st.db.writer.hold` | Histogram | `s` | `st.db.writer.origin`; acquisition through release, including loans and batched writes |
| `st.db.writer.current_hold` | Observable gauge | `s` | `st.db.writer.origin`; current hold age, zero when idle |
| `db.client.connection.wait_time` | Histogram | `s` | `db.operation.name=read`; reader checkout wait |
| `st.db.readers.open` | Observable gauge | `{connection}` | —; current open readers |
| `st.db.readers.idle` | Observable gauge | `{connection}` | —; current idle readers |
| `st.db.readers.opened` | Counter | `{connection}` | —; readers opened |
| `st.db.read.loan.oldest_age` | Observable gauge | `s` | `origin`; oldest live pooled connection loan per bounded route/task origin, not confirmed transaction age |
| `st.db.wal.size` | Observable gauge | `By` | —; stat the main database's `-wal` file |
| `st.db.wal.checkpoint.duration` | Histogram | `s` | `db.operation.name=checkpoint.truncate`; truncate checkpoint duration |
| `st.db.wal.checkpoint.deferred` | Counter | `{attempt}` | —; physical checkpoint attempts with nonempty WAL not recycled |
| `st.db.wal.checkpoint.lag` | Observable gauge | `{frame}` | —; last known WAL frames minus backfilled frames, summed across live pools |
| `st.db.wal.checkpoint.deferred_age` | Observable gauge | `s` | —; age since the first consecutive deferred attempt, maximum across live pools |

Every storage metric carries `db.system.name=sqlite`. Writer hold, queue, and oldest-loan origins reuse
the bounded writer-span vocabulary: normalized router `MatchedPath` templates prefixed
by one of nine HTTP methods, the fourteen fixed task names in the storage origin table,
and `other` for unknown origins. No paths, SQL, ids, or raw request URIs are attributes.
Each batch's hold uses its first queued operation's origin for its entire lifetime;
loans use the borrowing caller's origin. When multiple stores share an origin, the
current-hold gauge reports their maximum age, and reports zero for previously seen
origins after release. Collection does not require storage phase spans to be enabled.
Receive, admission, repair, and replay retain `POST /v1/internal/replication/receive`
through the blocking request scope; deferred wake passes retain `POST /v1/internal/replication-wake`.
The independent contention retry uses `task replication-projection-retry`. Startup uses
the existing `startup validate-replication-backlog`, `startup apply-replication-repairs`,
and `startup project-replication-backlog` labels. These are four additional closed values,
not dynamic task labels or new origin sources.
Wait and hold histogram boundaries are `0.001`, `0.005`, `0.01`, `0.025`, `0.05`, `0.1`,
`0.25`, `0.5`, `1`, `2.5`, `5`, `10`, `15`, `30`, `60`, `120`, `300`, `600` seconds.
Other duration histograms use the seconds buckets above; batch size uses explicit
boundaries `1`, `2`, `4`, `8`, `16`, `32`, `64`, `128`, `256`. Storage metrics are not sampled.

The daemon exports `st.runtime.lag` as an observable gauge in seconds with no instrument
attributes. One Tokio sleep per second measures expected-versus-actual wakeup delay;
the SDK samples the maximum of the last delay and an overdue pending wakeup, so a
stalled runtime remains visible before the sampling task resumes.
Synchronous storage-only telemetry registers no lag instrument when no Tokio runtime exists.

### Write-worker admission

These process-wide instruments cover the asynchronous admission queue before a
writer-bound blocking callback starts, not the SQLite writer's separate batch queue.
They use the existing SDK meter provider and daemon resource identity.

| Instrument | Kind | Unit | Attributes / meaning |
| --- | --- | --- | --- |
| `st.write_workers.queue.depth` | Observable gauge | `{job}` | Queued, not-yet-started top-level callbacks |
| `st.write_workers.queue.busy_age` | Observable gauge | `s` | Maximum per-store busy-period age: conservative upper bound on oldest queued age, not exact; zero when empty |
| `st.write_workers.queue.wait` | Histogram | `s` | `origin`: normalized HTTP route when available, otherwise `other`; enqueue through callback start (or explicit admission grant) |
| `st.write_workers.abandoned_callback_starts` | Counter | `{job}` | `outcome=ran`: callbacks starting after their request waiter was dropped |

Inherited dependent callbacks do not add queue entries. Pending admission cancellation
removes the entry; admitted callbacks remain queued until their blocking thread starts.
Explicit `admit_write` calls leave the queue when granted capacity. Each store tracks
depth and a busy-period start using atomics, without a per-write mutex, map allocation
or job scan. Across stores the gauges sum depth and take the maximum busy age.

On a 0-to-1 depth transition, an atomic minimum publishes the busy start. On the
1-to-0 decrement, the start is cleared, then depth is rechecked: if a new enqueue
raced the clear, an atomic minimum restores the earlier start. Acquire/release
operations order depth transitions and busy-start publication; overlapping transitions
may conservatively retain an earlier start. Empty observations always emit zero.
Busy age is an upper bound, **not exact oldest age**: a continuously nonempty queue
never resets even if its oldest items have started, and overlapping empty transitions
can retain an earlier start. Thus false-positive alerts are possible. A busy queue
observed before its start is published uses process-local queue uptime as the bound.
The wait histogram remains unchanged.

Mutation handlers retain their existing detached completion behavior. A closed response
oneshot detects abandonment before each synchronous handler poll; it only counts and
never cancels or skips work. Consequently the counter counts callback starts, not unique
HTTP requests; a pending handler can contribute again on a later abandoned poll.
`outcome=skipped` is reserved for jobs skipped after abandonment; the current implementation
does not skip jobs and emits only `ran`. Wait histograms use the storage duration buckets.

An alert for potentially old queued work uses the conservative upper bound:

```promql
max by (host_name) (st_write_workers_queue_busy_age_seconds * on(instance) group_left(host_name) target_info{job="infra/st-daemon"}) > 5
```

Proof: `worker_queue_exports_wait_and_abandoned_callback_starts` blocks worker capacity,
checks exported depth and busy-age bound, drops the requester before start,
and checks the exported abandonment counter, wait histogram, and empty-queue reset.
It also checks pending-admission cancellation removes queue entries.
`busy_period_covers_all_queued_callbacks_and_clears` checks busy-period publication,
retention across queued callbacks, clear and restart.

Loan age covers a connection checkout, which may have no active transaction; it does
not identify a WAL frame owner. An empty registry reports zero with origin `other`.
Checkpoint lag is the most recent successful PASSIVE report, not a WAL file-size estimate.
Deferral age continues between attempts and across errors (errors do not prove recovery);
an empty or recycled WAL resets it. A fully backfilled WAL awaiting recycling has zero
lag but can still have nonzero deferral age.

Replication export (`POST /v1/internal/replication/export`) retains its bulk read deadline,
but, like long-poll routes, has no handler-wide or nested blocking callback read loan.
Sealing and verdict writer waits therefore retain no reader. The snapshot-build gate
and snapshot-cache mutex waits check deadline/cancellation at most every 5 ms.
Authoritative inventory, graph and projection inputs remain captured together by
`sealed_replication_snapshot`'s explicit `read_snapshot`; payload/signature lookups
use scoped read guards. No snapshot is released and reacquired halfway through a build.
An expired export returns the existing `read-deadline` error, not a partial exchange;
the peer retries without acknowledging a partial inventory or advancing its cursor.

A scoped thread-local `Option<Arc<WriterWait>>` shares a request accumulator across
handler, store, action, and API blocking sections. Two relaxed atomics sum writer ACK
wait in nanoseconds and count writer operations. Outer handler completion records
`st.writer.wait_ms` (total ACK wait converted to milliseconds, only when at least 1 ms)
and `st.writer.ops` (operation count, including zero) on the existing SERVER request span.
Each scope restores the previous accumulator, including nested blocking sections;
writer calls do not create child spans
or perform per-operation span-context lookup. This preserves the single-span request
shape and adds no in-process sampling.

The daemon idle-WAL worker emits a detached INTERNAL `st.db.checkpoint` root only
when TRUNCATE was attempted or the result is abnormal. It uses the OTel API with
an explicit empty parent context, retaining the attempt's start time without constructing
a span for ordinary idle passes. `span.label` and `db.operation.name` are
`checkpoint.truncate` for TRUNCATE attempts, otherwise `checkpoint.passive`;
`db.system.name=sqlite`. A TRUNCATE result records `st.db.wal.checkpoint.duration`
in seconds, reusing the existing instrument vocabulary without adding a metric.
Failures, PASSIVE durations at least 1000 ms, writer waits or TRUNCATE durations
at least 100 ms, and at least three consecutive no-progress pinned-WAL samples
mark the root ERROR. Successful ordinary TRUNCATE leaves status UNSET.
Span emission is independent of the diagnostic log buckets and has no links.

## Reconciler and FIFO telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

```text
wake source → last recorded cause + coalesced Notify
                                  ↓ next outer wake
                     st.reconcile_pass / st.reconcile_deadline
                                  ↓ stabilization passes
                             cause=continuation

successful serialized channel send → FIFO enqueue Instant
real item received                 → remove oldest Instant before processing
SDK collection                     → sum live depths / max live oldest age
```

`st.reconcile_pass` and `st.reconcile_deadline` are independent root spans:
each uses `parent: None` and an empty OpenTelemetry parent context, so neither
inherits an ambient request or worker span. Their `span.label` values are
`pass` and `deadline`, respectively. `st.reconcile.items` records the desired
subject count on the span only; it is not a metric attribute. Errors set span
status to `ERROR`. These roots have no child or per-item spans and no local
sampling.

`st.reconcile.wake_cause` uses the following closed, case-sensitive snake_case identifiers.
The metric's `cause` attribute uses the same registry:

| Wake cause |
| --- |
| `api` |
| `replication_receive` |
| `deadline` |
| `timer_restart` |
| `timer_step_timeout` |
| `timer_resume_verification` |
| `timer_gate_timeout` |
| `timer_gate_recheck` |
| `timer_gate` |
| `timer_gate_poll` |
| `timer_llm_gate` |
| `file_watch` |
| `reconciler` |
| `recorder_receipts` |
| `startup` |
| `continuation` |
| `other` |

Notify coalesces wake requests, not an event backlog. The last recorded wake
cause is consumed on the next outer wake; subsequent stabilization passes use
`continuation`. Deadline inspection uses the previous outer wake cause, or
`startup` for the first check. A direct one-shot pass or deadline uses `other`,
as does an unknown source. These exported labels do not replace the existing
in-memory wake labels used by `/v1/performance` and `doctor`; request/detail
attribution also remains unchanged.

`st.reconcile.passes_per_wake` records the number of completed passes when an
outer wake rests or reaches its 64-pass continuation cap. It includes the
quiet confirmation pass, uses the outer wake's cause (not `continuation`),
and has count boundaries `1, 2, 4, 8, 16, 32, 64`. A wake rests when its
pass makes no writes of its own; unrelated concurrent store writes do not
request continuation. Writes completed on helper threads use their existing
notifications to request another outer wake.

| Instrument | Type | Unit | Complete attribute set |
| --- | --- | --- | --- |
| `st.reconcile.pass.duration` | Histogram | `s` | `task=pass` or `task=deadline` |
| `st.reconcile.wakes` | Counter | `{wake}` | `cause` from the 17-value wake registry |
| `st.reconcile.passes_per_wake` | Histogram | `{pass}` | `cause` from the 17-value wake registry |
| `st.fifo.depth` | Observable u64 gauge | `{item}` | `queue=writer`, `conversation`, or `terminal_emulation` |
| `st.fifo.oldest_age` | Observable f64 gauge | `s` | `queue=writer`, `conversation`, or `terminal_emulation` |

Every metric attribute is a constant closed value, never an identity.
These instruments contribute at most `17 + 17 + 2 + 3 + 3 = 42` attribute series;
histogram bucket count is separate. Duration uses the seconds buckets above,
and metrics are independent of trace sampling.

FIFO gauges describe only pending items in these in-process channels:

| `queue` | Channel and pending-item boundary |
| --- | --- |
| `writer` | Unbounded `std::sync::mpsc` writer channel; arrival-order batching and database lending preserve the channel's order |
| `conversation` | Unbounded WebSocket conversation channel per collection |
| `terminal_emulation` | Process-singleton unbounded job channel; pending jobs only, not emulation jobs already running |

`smallclaims::fifo` owns shared queue tracking. Each live channel owns an
`Arc<Queue>`; the registry retains only weak references, so dropping the queue
ends its observation. `Queue::send` serializes actual enqueue and successful
enqueue timestamp insertion under one mutex. A failed send adds no timestamp.
The receiver removes the oldest timestamp before processing the item. A writer
lending job prefetched behind a batch remains pending until the batch commits
and the connection is lent. Timestamps are `Instant` values; writer age reuses
the job's existing enqueue timestamp.

Gauges sample only at SDK collection: for each queue kind, depth is the sum
across live queue instances and oldest age is the maximum age across those
instances. Empty or absent queues report zero for both gauges. Collection
reads only tracker timestamps under their mutexes, never graph, database, or
file state. Tracking is a no-op without daemon OpenTelemetry initialization.

Excluded from FIFO telemetry are coalesced Notify/watch states, caches,
durable inbox and seat queues, driver-side mailbox `Subscription` queues, and
ping sidecar queues.

## Daemon startup telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

```text
node_identity::acquire → resolve stable node before telemetry initialization
Telemetry::init        → st.startup (local root, start backdated to acquisition)
                         └── st.startup.phase (one per phase, no chunk spans)
serving()     → close phase and root; batch export while the API serves
early return  → close phase and root with failed outcome and ERROR
```

`startup_telemetry::StartupTelemetry` starts after node identity acquisition and
telemetry initialization, with an explicit root start timestamp measured before acquisition.
The completed `node_identity` child uses acquisition's measured start and end.
Successful initialization uses the resolved stable node in the telemetry resource;
acquisition failure uses the configured node, closes the failed root and phase, flushes
telemetry, and returns the original error. Acquisition runs only once.
The root uses `parent: None` and an empty OpenTelemetry parent context.
`st.startup.outcome` is `serving` or `failed`; failure status uses the constant
description `startup failed`, not error prose or paths. `span.label` is `startup`.
Readiness closes both the last phase and the root without an explicit flush.
The collector retains local roots longer than one second, so slow starts are
kept. Startup has no local sampling.

The repository-owned phase registry maps the existing kebab-case startup names
to these closed snake_case values, shared by `st.startup.phase`, `span.label`,
and the duration metric's `phase` attribute:

| Phase |
| --- |
| `node_identity` |
| `install_hooks` |
| `open_store` |
| `judge_claims` |
| `validate_replication_backlog` |
| `apply_replication_repairs` |
| `settle_runs` |
| `project_replication_backlog` |
| `initialize_runtime` |
| `start_services` |
| `bind_listeners` |
| `other` (unknown-name fallback) |

Except for the retrospectively recorded `node_identity` phase, each phase starts
at its readiness transition and ends at the next phase or readiness/failure.
The active phase receives ERROR on failure. Existing `profile::task` boundaries remain unchanged inside
their phase; no entered tracing guard crosses an await or enters spawned
services. `start_services` covers background-service setup before listener binding.
The projection phase records integer `st.startup.claims_processed` and boolean
`st.startup.full_replay`. Incremental progress counts actual committed claims,
not store-index distance; full replay reports base-claim progress from the runtime.
These numeric/boolean observations are span-only, never metric labels.

`st.startup.duration` is a seconds histogram, recorded once per phase and once
with `phase=total` per start, including failed starts. Its explicit boundaries
are `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`,
`30`, `60`, `120`, `300`, `600`, `1200`, `1800`. The metric contributes at most
13 attribute series and is independent of trace sampling.

The otelite receiver proof observes the completed serving root before stopping
the daemon, eleven direct child phase spans, projection attributes, and duration
points for `total`, `node_identity`, and `open_store`. The root starts at or before
the identity phase. A database path occupied by a directory proves identity recovery
failure exports `failed` with ERROR on the root and phase; a readable database with
an unsupported schema version proves the separate `open_store` failure.
Unit proofs cover all call-site phase mappings, explicit acquisition timestamps,
and root completion on serving or guard drop.

## Replication worker telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

```text
exchange(peer) → st.replication.round (detached root, one per exchange call)
```

The root uses the OpenTelemetry API's `start_with_context` with an empty
parent context. smallclaims depends on the OTel API only; st3 owns providers.
The round does not inherit an ambient daemon or worker span. `span.label` is
the bounded operation `round`.
Span fields use the `st.replication.*` prefix. The round records `peer`,
`outcome=moved|in_sync|failed|cancelled` and integer `moved_envelopes`.
`moved` follows the exchange's existing progress result,
including checkpoint adoption; it need not imply a nonzero envelope count.
Envelope counts sum locally stored pulls and successful pushes whose peer
inventory changed; push counts are sent batch sizes, not remote receipt counts.
Failed operations set status `ERROR`
without exporting raw error prose. There are no per-POST spans and no in-process
sampling.
Dropping an unfinished round, including a dialer aborted after target
removal, finalizes it as `cancelled` with unset status, not `ERROR`. Explicit
completion and Drop consume the same recording, so counters and durations
are recorded once; cancellation does not itself increment the error counter.

| Instrument | Type | Unit | Complete attribute set |
| --- | --- | --- | --- |
| `st.replication.rounds` | Counter | `{round}` | `peer`, `outcome=moved|in_sync|failed|cancelled` |
| `st.replication.round.duration` | Histogram | `s` | `peer`, `outcome=moved|in_sync|failed|cancelled` |
| `st.replication.batch.size` | Histogram | `{envelope}` | `peer`, `direction=pull|push` |
| `st.replication.errors` | Counter | `{error}` | `peer`, `reason=down|overloaded|auth_failed|refused|removed|timeout|invalid|other` |

Durations cover the whole call, including failed and cancelled calls, and use the seconds
buckets above. Pull batch sizes are locally stored envelopes; push sizes are
successfully sent envelopes. Neither counts signatures or claims.
`peer` is bounded by fleet members plus explicitly
configured/bootstrap peers; it is a node name, never a URL, route, session,
agent, or message id. Outcomes, directions, and reasons are closed
lowercase registries owned by Fractal. No other identities become metric
attributes.

Error reasons preserve worker failure classification: typed `PeerOverloaded`
maps to `overloaded`, `RemovedFromFleet` to `removed`, and
`FabricGrantRefusal` to `refused`. Reqwest timeouts map to `timeout`, other
reqwest failures to `down`, and JSON decoding, API-version mismatch, and
inflate failures to `invalid`. The legacy dialer's signature/fleet/member
authentication classification maps to `auth_failed`; unclassified errors map
to `other`, while recognized legacy failure-kind strings retain their mappings.
A signed HTTP refusal does not alone imply `refused`. Reasons never include error text.

Trace and metric enabled gates are independent: metrics record without a
trace exporter, and traces record without a meter provider. Disabled paths
return before constructing spans, instruments, attribute collections, or
telemetry-only timers/counts. An enabled operation owns one refcounted peer
label shared between its span and metrics; only metrics require a monotonic
start time. Batch/outcome/reason labels use static strings, and point
recording shares the peer label without copying its bytes.
Telemetry handles bind to the first provider in the process; production initializes once per process.
Enabled paths reuse existing exchange and receipt data rather than
querying the daemon for telemetry. `ReplicationReceipt` has no timing fields;
cumulative daemon `ReplicationTimings` are not fetched or turned into per-round
timings. No new IPC or derived timing math is added.

There are no replication backlog or lag gauges: the worker has no
`peer_only`/`local_only` counts and the store belongs to the daemon. This
surface adds no store scans or queries to manufacture those observations.

## Daemon process and allocator memory

The `st-daemon` SDK meter registers these instruments only when metrics export is enabled.
The CLI, hooks, and replication worker do not register them. The SDK's periodic reader
samples them at export time (60 seconds by default; the standard
`OTEL_METRIC_EXPORT_INTERVAL` override applies). They share the existing resource and
have no point attributes except the heap gauge's closed `state` vocabulary.

| Instrument | Type | Unit | Source / attributes |
| --- | --- | --- | --- |
| `st.process.memory.rss` | Observable gauge | `By` | Linux `/proc/self/statm` resident pages × system page size; no attributes |
| `st.allocator.heap` | Observable gauge | `By` | Linux glibc `mallinfo2`; `state` = `allocated`, `free`, `mmapped`, `arena`, `releasable` |
| `st.allocator.reclaim.count` | Observable counter | `{attempt}` | Process-lifetime checkpoint reclamation attempts; no attributes |
| `st.allocator.reclaim.duration` | Observable counter | `s` | Cumulative checkpoint reclamation elapsed time; no attributes |

One heap callback calls `mallinfo2` once and emits all five states. glibc visits
arenas under their locks, not at an atomic all-arena instant. `allocated` is
`uordblks`, allocated non-mmapped heap bytes including allocator overhead and
tcache-retained chunks, not live application payload.
`free` is `fordblks`, free bytes retained inside arenas; `mmapped` is `hblkhd`,
separate mmapped allocations. `arena` is the total non-mmapped bytes obtained from
the system and overlaps `allocated` and `free`. `releasable` is `keepcost`, an
estimate of the main arena's top-most releasable block, not all free or reclaimable
pages across arenas. These states are not a mutually exclusive partition.
RSS also includes non-heap memory. The direct `mallinfo2` link requires glibc 2.33
or newer for the Linux GNU binary even when metrics export is disabled; the documented
Linux archive runtime floor is glibc 2.35+. RSS is registered only on Linux, and
the heap gauge only on Linux GNU targets; unsupported targets do not report
fabricated memory values. An unreadable RSS sample is omitted.

The smallclaims public `allocator_reclaim_stats()` accessor reads relaxed atomic totals
for attempts, actual native trim calls, and cumulative elapsed time without calling the
allocator. An attempt occurs when an armed outermost checkpoint completion scope drops;
nested work triggers one attempt, and skipped work triggers none. The exported count
is attempts (including no-op unsupported platforms), not trim successes. The existing
thread-local test counters remain isolated from concurrent completion proofs.
The producer still takes the existing pair of clock readings and updates relaxed
atomic totals when metrics export is disabled; only SDK registration and collection
are opt-in. Observation adds no reclamation calls or changes to checkpoint cleanup.

Proof: the completion-scope unit test checks production totals increase. Source
review verifies the callback's single `mallinfo2` call. The Linux GNU `otel_export`
integration test captures all five closed states with a common collection timestamp
and positive `allocated` heap and RSS from an isolated daemon using `test_bin!("st3")`;
the timestamp proves a common SDK collection, not the number of native calls.
A fresh-test-process proof initializes real daemon telemetry with traces enabled and
metrics disabled, asserting no meter provider is created; it does not infer absence
from elapsed time or daemon termination. A companion CLI proof checks non-daemon omission.
The ignored `mallinfo2_fragmented_heap_cost` test measures
101 snapshots after allocating and touching about 1 GiB in 4 KiB blocks and freeing
alternate blocks; it reports median, p95, and maximum elapsed time without touching a
live daemon.

## Attribute and context policy

Agent, session, message, terminal, attachment, and lease ids are span attributes only:
never metric labels or `span.label`. Capabilities and capability hashes never enter traces,
metrics, or logs. Span names and labels use bounded operation vocabulary. Existing
`profile::Op` and `profile::task` labels supply that vocabulary where available.

W3C `traceparent` and `tracestate` are the wire context, not hash-derived identities.
`crates/st3-client/src/propagation.rs` supplies one injector for both the CLI's internal
daemon client and the generated typed Rust client. Every HTTP request, Unix HTTP request,
and WebSocket handshake injects the current tracing span's OpenTelemetry context through
the global text-map propagator. This makes the daemon's SERVER span a child of the CLI's
`st3.cli.command` span (or its current child); hook requests use their current span in the
same way. No SDK/OTel layer or an invalid context produces no trace headers.

The wire-only tracestate entry `st=c` is owned by Fractal and means sampling is decided
by the collector. Injection replaces an existing `st` entry without growing the list.
When adding `st` to a full 32-entry list, it first drops the rightmost entry; all other
entries retain their order. It does not mutate the current span's context. The daemon
treats any present `st` key as collector-owned sampling, so only sampled external callers without that key set
`st.parent.sampled=true`.

The typed Rust client's optional `trace-propagation` feature owns its OTel API and tracing
bridge dependencies, not an exporter or SDK initializer. `st3` enables the feature;
`stui` and the client TUI example leave it disabled. Without the feature, injection is
a no-op with no context lookup, trace headers, or OTel dependencies. The embedding
application owns subscriber and propagator setup. The client source is generated from
`crates/st3-client-codegen/templates/lib.rs.in`; the
shared propagation module is handwritten. Invalid-context injection performs only a
current-span/context check, with no allocation, header formatting, or propagator lookup.
Valid-context injection clones/updates tracestate and uses the propagator to format the
two W3C headers directly into each transport's existing request, without an intermediate
header collection.

Peer propagation is pending and is not enabled by these injectors. When implemented,
peer context belongs inside the `FleetAuth`-signed header set. The current integration boundaries are described below.

## Proof and overhead

The core receiver proof uses `otelite` to inspect trace, metric, and correlated log export,
process identity and version, and the unset-endpoint no-export control. Trace proofs cover
export of fast roots and spans with unsampled remote parents; metrics record independently.
The daemon request proof checks caller trace continuity, `service.name=st-daemon`, and
`st.parent.sampled=true`/`false` for sampled and unsampled remote parents respectively,
the single-span shape (phase attributes present, no admission/handler child spans), and
that a healthy request exports no below-WARN log record. A unit test pins the batch queue
bounds.
The storage-stage receiver proof exports request totals at 0, 0.999, 1, and 1.001 ms,
checks the 1 ms attribute boundary and unchanged operation counts, and forces a real
TRUNCATE using unpinned WAL frames in an isolated store. It checks ordinary passes emit
no root, abnormal/failure results are ERROR, and roots remain parentless even with
both an entered tracing span and an attached OTel context.

The stage-span receiver proof compares `st.request.id` with the HTTP envelope, checks
roster/page stage values, and links first-frame roots to the exported upgrade trace/span
ids. It asserts that repeated rereads or changes produce no additional first-frame
roots, cold roster builds are detached and unlinked, and receive projection roots are
detached but linked to the receive request. Empty receives produce no projection root.

The CLI-to-daemon receiver proof runs a real command with both processes exporting,
asserts the daemon SERVER span has the CLI root's trace id and a CLI parent span id,
and checks st-marked parents do not set `st.parent.sampled=true`. Builder unit tests pin
both headers under a valid current context and their absence without an OTel layer.
The CLI shutdown helper is tested with an exporter that never returns from shutdown:
the caller reports a receive timeout and writes the negative cache within the 50 ms
deadline plus 200 ms of scheduling/filesystem tolerance. The process-level black-hole
collector proof checks successful and failed commands exit within 10 s despite a 30 s
exporter timeout, write the cache, and make no new collector connection on the next
call within the backoff window. It does not compare whole-process timing medians.

The receiver tests exercise transport and attribution contracts. They do not certify
the current production CPU, p99, RSS or sampled-traffic budgets. Performance evidence
must identify the source, store, load and collector configuration separately.

## Integration boundaries

Peer trace propagation is not enabled. Adding context to the signed peer protocol
requires a separate protocol decision and mixed-build verification.

`ST3_PROFILE_DIR` artifacts remain separate from SDK exports. Whether they should
share coverage or vocabulary remains a design question.

The daemon has no SIGTERM flush handler. A service stop can therefore lose its last
buffered telemetry batch; the five-second budget bounds an invoked shutdown, rather
than promising that SIGTERM invokes one. The lifecycle policy remains open.

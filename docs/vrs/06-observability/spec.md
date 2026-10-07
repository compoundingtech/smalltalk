# Observability specification

This document owns the mechanism behind [requirements](requirements.md): the crate stack, exporter
configuration, span roots, unit propagation, test strategy, and delivery order. Naming,
provenance, and span-label rules are referenced from the dotfiles context `observability` tree
(`01-conventions`); the six producer obligations from its `09-integration` spec. st2-side
obligations land here; registry/dashboard/census obligations are deferred cross-repo
(O11Y-R08).

## Crate stack

```
opentelemetry                   = "0.30"
opentelemetry_sdk               = { version = "0.30", features = ["logs"] }
opentelemetry-otlp              = { version = "0.30", default-features = false,
                                    features = ["http-json", "reqwest-blocking-client",
                                                "internal-logs", "logs"] }
# tracing facade (PR3) — spans and logs unify on one subscriber
tracing                         = "0.1"
tracing-subscriber              = "0.3"
tracing-opentelemetry           = "0.31"
opentelemetry-appender-tracing  = { version = "0.30",
                                    features = ["experimental_use_tracing_span_context"] }
```

The appender-tracing `experimental_use_tracing_span_context` feature is load-bearing: without
it, emitted log records do not receive the active tracing span's trace/span ids and every
record exports uncorrelated.

The otlp feature set is load-bearing, not stylistic:

- **No gRPC client.** The fleet pipeline is OTLP/HTTP JSON only (`otel-stack.md`); no gRPC clients
  anywhere.
- **Blocking reqwest client only.** With both `reqwest-client` and `reqwest-blocking-client`
  enabled (the defaults include blocking alongside async), the crate compiles but every runtime
  client-selection cfg arm requires *not*-having the other feature, so export fails with
  `NoHttpClient` at span-export time. Exactly one of the two must be enabled.
- **Blocking chosen over async** because st2 has no tokio reactor; the async batch exporter
  panicked without one. See the prototype evidence
  ([.experiments/2026-08-25-rust-to-otelite-capture.md](.experiments/2026-08-25-rust-to-otelite-capture.md)).
- **`internal-logs`** keeps exporter-internal errors observable instead of swallowed.

## Exporter and provider setup

One module, `src/telemetry.rs`, owns init and teardown via `Telemetry::init(unit)` /
`Telemetry::shutdown()`:

- **Endpoint**: none configured in code. The exporter resolves `OTEL_EXPORTER_OTLP_ENDPOINT` and
  related `OTEL_*` variables from the environment automatically. Unset → no SDK provider or
  exporter is built (R02); since PR3 the human-readable stderr `tracing` layer still runs so
  diagnostics stay visible — see [Log bridge](#log-bridge-pr3-landed).
- **Protocol**: HTTP JSON (`http-json` + protobuf-free wire), batch exporter, targeting the local
  Alloy forwarder at `127.0.0.1:4318` by convention.
- **Resource**: `service.name` = `st2-<unit>` selected per entrypoint (below; `src/main.rs`
  passes `supervisor`, `hook`, or `cli`), `service.version` from `crate::version::machine_version`, and
  `host.name` from the existing host detection. The remaining R04 fleet attributes
  (`service.namespace`,
  `service.instance.id`, `sk.site`, `sk.role`, `deployment.environment.name`) are not set yet —
  tracked as an [open question](open-questions.md).
- **Flush/shutdown**: `force_flush` + global `shutdown` registered to run at process exit. The
  batch exporter buffers; without explicit flush at exit, tail spans of short-lived CLI runs are
  lost. This pairing is required for delivery, not optional.

### service.name values (R05)

| Process unit | `service.name` |
| --- | --- |
| Supervisor loop (`st2 up` daemon / systemd unit) | `st2-supervisor` |
| One-shot CLI invocations | `st2-cli` |
| Hook executions (`st2 driver claude-observe`; other hook surfaces not instrumented yet) | `st2-hook` |
| Claude's status-line tee (`st2 driver claude-statusline`) | **none — no pipeline is built** |

**The one deliberate exemption, and the rule behind it.** A subcommand whose cadence is set by a
harness's refresh timer rather than by an operator or an event does not initialize the telemetry
pipeline at all (`Telemetry::local_only`). Claude's status-line tee is the only such surface
today: `refreshInterval: 5` makes it ~720 short-lived processes per hour per seat, and Claude
waits for each to exit, so the final collect-and-export at shutdown would sit in the render path.
Measured against a bound-but-never-accepting collector, a tee that builds a pipeline takes 5.0 s
on the path that logs and `claude-observe` takes 10.0 s, against 0.01–0.06 s with none
(`08-harness-context`, `DQ-C13`).

The rule is about **cadence, not about being a hook**: `claude-observe` is event-driven, is named
in the table above, and stays instrumented. Anything added to the exempt list needs the same
argument — a harness-driven repeat rate and no operation worth a span — not merely being a
hook-set script.

## Reconciliation trace hierarchy

Instrumented exclusively through the `tracing` facade (`tracing::info_span!` plus
`tracing-opentelemetry` status extension), one bounded trace represents one reconcile pass:

```
st2.reconcile_pass
├── st2.catalog.lock
├── st2.catalog.discover
├── st2.hooks.verify                 # only when a consumer requires hooks
├── st2.catalog.materialize
├── st2.runtime.observe              # omitted for an externally supplied snapshot
└── st2.reconcile.execute
```

`st2.reconcile_pass` remains the compatibility root at the supervisor-loop, one-shot catalog,
selected-task, and single-file-spec sites. Its `span.label` and `st2.reconcile.path` are the enum
`catalog | selected | spec`. Root outcome attributes are `st2.host`, `st2.crash_loops`,
`st2.unparked`, `st2.report.errors`, `st2.report.warnings`, `st2.reconcile.skipped`, and
`st2.result = pass | fail`; non-empty errors set OTel status `ERROR`. A deterministic INFO event
(target `st2`, message `reconcile pass complete`, `result = pass | fail`) closes every root so
log-based assertions need no fault injection.

| Span name | Parent | `span.label` | Operation boundary | Attributes and status | Path applicability |
| --- | --- | --- | --- | --- | --- |
| `st2.reconcile_pass` | none | `catalog` \| `selected` \| `spec` | One complete reconcile pass | Root attributes above; `ERROR` when the pass returns/collects an error | Catalog loop/once, selected task, spec loop/once |
| `st2.catalog.lock` | `st2.reconcile_pass` | `shared` | Shared catalog-authoring lock acquisition | `st2.result`; `ERROR` on acquisition failure | Catalog, selected |
| `st2.catalog.discover` | `st2.reconcile_pass` | `catalog` | Recursive desired-state snapshot | `st2.catalog.spec_count`, report warning/error counts, `st2.result`; `ERROR` when discovery reports errors even though the pass may continue | Catalog, selected |
| `st2.hooks.verify` | `st2.reconcile_pass` | `lifecycle hooks` | Required lifecycle-hook receipt/set verification | `st2.hooks.consumer = codex \| pi \| codex+pi`, `st2.result`; `ERROR` on verification failure | Catalog or selected, only when required |
| `st2.catalog.materialize` | `st2.reconcile_pass` | `catalog` \| `selected owner` | Aggregate catalog/selected-owner materialization call | Materialization failure and report warning/error counts, `st2.result`; `ERROR` when materialization reports errors | Catalog, selected |
| `st2.runtime.observe` | `st2.reconcile_pass` | `all sessions` | Authoritative `Runner::list_sessions` call | `st2.runtime.session_count`, `st2.result`; `ERROR` on list failure | Catalog, selected, spec; omitted by `_with_sessions` because that snapshot is external |
| `st2.reconcile.execute` | `st2.reconcile_pass` | `apply plan` | Aggregate mutation call around `execute_with_presentation_cursor` | Plan launch/GC/teardown counts, newly added report warning/error counts, `st2.result`; `ERROR` only when execution adds errors | Catalog, selected, spec |

Every first-party root and child has a non-empty `span.label`. The exporter-enabled
`AtomicBool` in `src/telemetry.rs` is the hierarchy gate; `tracing::enabled!` is insufficient
because the stderr formatter remains installed without an endpoint. When the tracer exporter is
unset, child constructors return before span construction, label handling, collection allocation,
or count inspection. All children are aggregates and trace volume is bounded by the table.

Attribute policy follows the central `01-conventions` contract:

| Attribute family | Value type | Cardinality | Privacy | Metric-label policy |
| --- | --- | --- | --- | --- |
| `span.label` | enum string | bounded | public | forbidden |
| `st2.reconcile.path`, `st2.result`, `st2.hooks.consumer` | enum string | tiny/bounded | public | spanmetrics-only |
| All `*_count`, `st2.crash_loops`, `st2.unparked`, `st2.report.errors`, `st2.report.warnings` | integer | bounded numeric | public | forbidden |
| `st2.reconcile.skipped` | boolean | tiny | public | spanmetrics-only |
| `st2.host` | string | bounded fleet identity | internal | forbidden |

No span or status description carries an id, filesystem path, selector, or error prose.

Explicitly rejected spans: pure reconcile planning, identity validation,
`compile_generated_tasks`, debounce, report absorption, wait/sleep, watcher callbacks, and
wrapper functions. Per-task and per-owner spans are also rejected from this hierarchy: they need
a separately specified hard detail budget. Provider-session lifecycles and exec sidecars remain
follow-up surfaces beyond the PR2 launch/reap counters.

### Native-driver diagnostic transitions (O11Y-R09)

`src/driver_diagnostic.rs` emits one `st2.driver.diagnostic` span plus its
correlated `st2 native driver diagnostic transition` INFO event only when a
typed failure tuple changes or recovers. `span.label` is the closed stage.
Span/event attributes are `st2.driver.stage`, `st2.driver.reason`,
`st2.driver.source`, `st2.driver.support`, and `st2.outcome`; the raw
`st2.driver.producer_version` is span/log-only. No agent, runtime, session, or
message id is needed on this transition, and no prompt/message/path value is
recorded.

The span's stage/reason/source/support/outcome attributes are the same closed
values used by the counter below. Versions and identities are specifically not
counter labels or `span.label`. With no trace exporter the span is not
constructed; with no meter provider the counter returns before touching its
instrument.

## Metrics (PR2)

Landed RED-minimal set per interview decision Q5; every label value comes from a bounded enum,
and identifiers never become metric labels (ids stay in span attributes). `src/metrics.rs` owns
the instruments; every record call early-outs unless a meter provider is installed.

| Instrument | Type | Labels |
| --- | --- | --- |
| `reconcile_passes_total` | counter | `result` = `pass` \| `fail` |
| `task_launches_total` | counter | `driver` = `codex` \| `claude` \| `opencode` \| `pi` \| `omp` \| `exec` \| `other` |
| `task_reaps_total` | counter | `driver` (same enum as launches) |
| `hook_invocations_total` | counter | `hook` = registry name (`claude-observe`), `event` = bounded Claude hook-event set, unknown → `other` |
| `message_deliveries_total` | counter | `result` = `pass` \| `fail` |
| `crash_loops_total` | counter | — |
| `driver_diagnostic_transitions_total` | counter | `stage`, `reason`, `source`, `support`, `outcome = failure | recovery` (all closed enums) |
| `resource_observe_requests_total` | counter | `outcome` = `accepted` \| `backpressured` \| `settledUnchanged` \| `settledChanged` \| `settledFailed` \| `absentBinding` \| `staleGeneration` \| `providerUnavailable` \| `other` |
| `resource_observe_dispatch_seconds` | histogram | — |
| `resource_observe_settle_seconds` | histogram | — |
| `reconcile_pass_duration_seconds` | histogram | — |
| `session_start_duration_seconds` | histogram | — |

All duration histograms share seconds-scale explicit bucket boundaries
`0.001`, `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.25`, `0.5`, `1`,
`2.5`, `5`, and `10` (`DURATION_BUCKET_BOUNDARIES` in `src/telemetry.rs`)
instead of the SDK's millisecond-tuned defaults. This keeps sub-second
reconcile passes, spawns, observe dispatches, and settlements distinguishable.
Observe metric statuses use the same camelCase durable-wire spelling; kebab-case
is reserved for human CLI text.

Scope notes: passes are counted at all three `st2.reconcile_pass` sites (catalog loop pass,
one-shot up, and the single-file spec path — `reconcile_pass_specs_with_sessions`, which now
emits the same root span shape); `fail` means the pass collected errors. Reaps count the
restart path in the launch loop, where driver context exists. Deliveries cover bus deliveries
onto a recipient inbox (`deliver_record`, send + retry paths); ding/native transport outcomes
are separate follow-ups. Hook invocations are observed at the single in-process application
point (`st2 driver claude-observe`); hook scripts the harnesses execute directly are not
visible to st2. The `driver` label is a closed enum resolved by precedence: `exec` task kind
first, then a typed driver declaration, then an observational argv/shell token heuristic
(alphanumeric tokens matched in launch order: `codex`, `claude`, `opencode`, `omp`, `pi`; anything
else → `other`). Because the heuristic inspects arbitrary user work, a hand-authored seat may
be labeled by what its command line merely mentions — the label is diagnostic only and never
influences reconcile decisions.

The meter provider shares PR1's plumbing: `Telemetry::init` installs an `SdkMeterProvider`
with a `PeriodicReader` + OTLP/HTTP-JSON metric exporter behind the same
`OTEL_EXPORTER_OTLP_ENDPOINT` guard and resource; unset → no provider and the global meter is
a silent no-op (R02 zero-overhead). `Telemetry::shutdown` force-flushes metric points alongside
spans so short-lived CLI runs deliver them.

## Log bridge (PR3, landed)

Resolved by interview decision Q6: the `tracing` facade unifies spans and logs on one
subscriber (`src/telemetry.rs`):

- **stderr fmt layer — always installed.** Human-readable lines keep today's diagnostics
  visible with or without an endpoint. This is a deliberate deviation from PR1's literal
  zero-output unset-endpoint behavior: migrated `tracing` sites must not go silent. Level
  filtering defaults to INFO; `RUST_LOG` overrides.
- **Span layer** — `tracing-opentelemetry` exports spans through the existing tracer provider.
- **Log bridge** — `opentelemetry-appender-tracing` exports events through a new SDK logger
  provider sharing the endpoint, HTTP-JSON protocol, blocking client, and resource. With the
  `experimental_use_tracing_span_context` feature, records emitted inside a span carry its
  trace/span ids.

`Telemetry::shutdown` force-flushes and shuts down logger, meter, and tracer providers together.

Emission-site migration rule: non-user-facing diagnostics (`eprintln!` warn/error paths in
crash-loop handling, park-channel setup, catalog watching, ding transport ambiguity, driver
session degradation) became `tracing::warn!`/`error!` with unchanged message text. USER-FACING
CLI OUTPUT STAYS `println!`/`eprintln!`: command results (`installed`, boot reports, `ls`
tables), lock banners, and validation reports are interfaces, not diagnostics.

## Systemd unit propagation

`src/service.rs` builds the supervisor unit and serializes the operator's `OTEL_*` environment
into `Environment=` lines alongside the existing `PATH`/`PTY_ROOT` serialization, so
`st2 up --install-unit` preserves R02 (ambient endpoint) under systemd. Unit tests in
`service.rs` extend the existing serialization assertions.

## Testing strategy

- **Integration tests** (`tests/otel_export.rs`, cargo integration tests): the receiver is a
  prebuilt `otelite` binary passed by path via `ST2_OTELITE_BIN` (the effect-utils flake package
  output; gate wiring supplies it, and a gate run hard-fails without it unless
  `ST2_ALLOW_OTEL_SKIP=1` explicitly allows a local skip). Each test spawns
  `otelite capture` on an ephemeral port (`--http-port 0`), points the binary under test at it
  via `OTEL_EXPORTER_OTLP_ENDPOINT`, drives one command, then stops the receiver by closing its
  stdin — EOF flushes the capture to disk — and asserts on the captured traces (span names,
  resource attributes), metrics (PR2), and log records (PR3: the deterministic
  `reconcile pass complete` INFO record must carry the `st2.reconcile_pass` span's trace/span
  ids, proving tracing→OTel correlation end to end). Precedent: dotfiles op-proxy tests use
  `captureEnvTrace`; dotfiles branchy checks consume
  `effect-utils.packages.<system>.otelite`.
  - Caveat baked into harness design: `otelite capture` treats stdin EOF as termination, so the
    harness closes stdin deliberately as the stop signal rather than leaking `/dev/null`.
- **No-op proof**: a test asserts that with `OTEL_EXPORTER_OTLP_ENDPOINT` unset, the command
  completes normally with no export activity — guarding R02.
- **Flake check wiring**: the check pulls effect-utils' `otelite` package output, mirroring the
  branchy-check pattern, so CI proves R03 end-to-end without network access to dev3.

## Delivery: gh stack of three PRs

1. **PR1 — traces.** SDK init, OTLP/HTTP-JSON exporter with the exact feature set above, resource
   attributes, trace roots, unit `OTEL_*` propagation, otelite-based integration tests and flake
   check wiring, plus this VRS tree.
2. **PR2 — metrics.** Metric set finalized per open questions; shares provider/exporter/resource
   plumbing from PR1; otelite assertions extended to metrics.
3. **PR3 — log bridge.** Landed per Q6: `tracing` facade adopted, spans and logs on one
   subscriber; correlated diagnostics migrated; otelite assertions extended to logs.

Each PR lands CI-green independently; PR2/PR3 depend on PR1's plumbing only.

## st3

This section specifies the st3 core mechanism (O11Y-R10–R18). The st2 sections above retain
their own process model. The design source is [#1580](https://github.com/compoundingtech/smalltalk/issues/1580).

```text
process tracing ── AlwaysOn ── batch span processor ───────────┐
metric instruments ── periodic reader ────────────────────────┼── SDK threads ── OTLP/HTTP JSON
tracing events ── correlated log bridge ── batch processor ───┘
hook signals ── daemon observations exporter ────────────────────────────────── OTLP/HTTP JSON
```

### Pipeline and identity

`crates/st3/src/otel.rs` owns SDK initialization, resource construction, and shutdown.
The tracer provider uses `Sampler::AlwaysOn` and a plain SDK `BatchSpanProcessor` to export
every span, including spans with an unsampled remote parent. There is no in-process sampler
or span buffer beyond the SDK batch queue. The pipeline uses the crate versions and
blocking-only OTLP feature set listed above.
The batch queue is bounded below the SDK defaults because O11Y-R18 also caps daemon RSS:
`max_queue_size` 256 and `max_export_batch_size` 256, with the SDK's default
`scheduled_delay` (5 s, `OTEL_BSP_SCHEDULE_DELAY`). The SDK's 2048/512 defaults measured
+60 MiB RSS at saturation (queue of `SpanData` plus the exporter's in-flight OTLP/JSON
batch and reqwest buffers); 256/256 keeps the worst case — one in-flight batch of at most
the queue's spans — inside the +32 MiB budget. A 64-span batch measured within the RSS
budget but its four-times-higher POST rate alone exceeded the +2% CPU/request budget at
saturation, so the batch equals the queue. The `BatchConfigBuilder` setters override the
environment, so `span_batch_config` re-applies `OTEL_BSP_MAX_QUEUE_SIZE` and
`OTEL_BSP_MAX_EXPORT_BATCH_SIZE` explicitly — those variables keep working. A full queue
drops spans; the SDK counts the drops and reports the first drop plus the shutdown total
through its internal `otel_warn` diagnostics (`BatchSpanProcessor.SpanDroppingStarted`),
which the stderr layer prints. There is no exported drop counter in this PR.
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

`crates/st3/src/telemetry.rs` remains the hook path. Hooks hand signals to the daemon and
never contact a collector. The observations exporter emits hook spans and hook invocation
metrics as `st-hook`; observation logs and usage metrics retain `st-daemon`. The statusline
cadence exemption remains the DQ-C13 rule above.

The shared resource contains `service.name`, `service.version` from
`st_drivers::version::machine_version()`, a random per-process `service.instance.id`,
`host.name`, and `st3.node`. The observations exporter in `crates/st3/src/otlp.rs` uses this
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

### Collector sampling policy

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

### Server request spans

`response_envelope` constructs one local root span per request only when trace export is
enabled. Its name is `"{METHOD} {route}"`, where `route` is the matched route template or
`/unmatched`, never the raw path. The request is a single span: at saturation five child
spans per request priced in well above the O11Y-R18 CPU budget, so the phases are numeric
attributes on the root instead.

| Attribute | Meaning |
| --- | --- |
| `st.admission.queue_ms` | Wait for the admission `spawn_blocking` slot (client requests) |
| `st.admission.authenticate_ms` | `authenticate` duration (client requests) |
| `st.admission.snapshot_ms` | Cursor snapshot duration (client requests) |
| `st.handler.queue_ms` | Wait for the handler `spawn_blocking` slot (non-health routes) |
| `st.handler.duration_ms` | Handler duration, including `/v1/health` |

Each is an integer count of milliseconds recorded when its phase ends; an attribute is
absent when the phase did not run for that request. The collector's slow-request and
error policies key on the root span's duration and status, which the single span preserves.

The root attributes are `http.request.method`, `http.route`, `http.response.status_code`,
`st3.client.class`, and `span.label` equal to the route. A 5xx response or a handler error
sets status `ERROR`; a 4xx response alone does not. WebSocket routes end the server span at
the 101 response, not when the socket closes.

The server extracts W3C `traceparent` and `tracestate` from HTTP request and WebSocket
upgrade headers and uses the extracted context as the parent. The sampled-caller signal
follows the collector sampling policy above; the process exports every span.

### Metric naming and cardinality

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

### Storage telemetry (O11Y-R14, O11Y-R18)

| Instrument | Type | Unit | Additional attributes / meaning |
| --- | --- | --- | --- |
| `db.client.operation.duration` | Histogram | `s` | `db.operation.name=write.batched` or `write.lend`; writer enqueue through ACK |
| `st.db.writer.queue.duration` | Histogram | `s` | —; enqueue through writer dequeue |
| `st.db.writer.batch.size` | Histogram | `{job}` | —; jobs in a writer batch |
| `st.db.writer.commit.duration` | Histogram | `s` | —; writer commit duration |
| `st.db.writer.batch.duration` | Histogram | `s` | —; whole writer batch duration |
| `db.client.connection.wait_time` | Histogram | `s` | `db.operation.name=read`; reader checkout wait |
| `st.db.readers.open` | Observable gauge | `{connection}` | —; current open readers |
| `st.db.readers.idle` | Observable gauge | `{connection}` | —; current idle readers |
| `st.db.readers.opened` | Counter | `{connection}` | —; readers opened |
| `st.db.wal.size` | Observable gauge | `By` | —; stat the main database's `-wal` file |
| `st.db.wal.checkpoint.duration` | Histogram | `s` | `db.operation.name=checkpoint.truncate`; truncate checkpoint duration |

Every storage metric carries `db.system.name=sqlite`. These are the complete attribute
sets: at most two combinations per instrument, with no paths, SQL, ids, or other
unbounded attributes. Duration histograms use the seconds buckets above; batch size uses
explicit boundaries `1`, `2`, `4`, `8`, `16`, `32`, `64`, `128`, `256`.
Storage metrics are not sampled.

A scoped thread-local `Option<Arc<WriterWait>>` shares a request accumulator across
handler, store, action, and API blocking sections. Two relaxed atomics sum writer ACK
wait in nanoseconds and count writer operations. Outer handler completion records
`st.writer.wait_ms` (total ACK wait converted to milliseconds) and `st.writer.ops`
(operation count) on the existing SERVER request span. Each scope restores the previous
accumulator, including nested blocking sections; writer calls do not create child spans
or perform per-operation span-context lookup. This preserves the single-span request
shape and adds no in-process sampling.

### Reconciler and FIFO telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

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

| Instrument | Type | Unit | Complete attribute set |
| --- | --- | --- | --- |
| `st.reconcile.pass.duration` | Histogram | `s` | `task=pass` or `task=deadline` |
| `st.reconcile.wakes` | Counter | `{wake}` | `cause` from the 17-value wake registry |
| `st.fifo.depth` | Observable u64 gauge | `{item}` | `queue=writer`, `conversation`, or `terminal_emulation` |
| `st.fifo.oldest_age` | Observable f64 gauge | `s` | `queue=writer`, `conversation`, or `terminal_emulation` |

Every metric attribute is a constant closed value, never an identity.
These instruments contribute at most `17 + 2 + 3 + 3 = 25` attribute series;
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

### Daemon startup telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

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

### Replication worker telemetry (O11Y-R10, O11Y-R11, O11Y-R14, O11Y-R16, O11Y-R18)

```text
exchange(peer) → st.replication.round (detached root, one per exchange call)
heal(peer)     → st.replication.heal  (detached root, one per heal call)
```

Both roots use the OpenTelemetry API's `start_with_context` with an empty
parent context. smallclaims depends on the OTel API only; st3 owns providers.
A heal does not inherit the round, and neither inherits an ambient daemon or
worker span. `span.label` is the bounded operation `round` or `heal`.
Span fields use the `st.replication.*` prefix. The round records `peer`,
`outcome=moved|in_sync|failed|cancelled`, integer `moved_envelopes`, and boolean
`heal_due`. `moved` follows the exchange's existing progress result,
including checkpoint adoption; it need not imply a nonzero envelope count.
Envelope counts sum locally stored pulls and successful pushes whose peer
inventory changed; push counts are sent batch sizes, not remote receipt counts.
The heal records `peer`, integer `questions`, and
`outcome=repaired|unchanged|failed|cancelled`. A heal is repaired when its graph matches
after refetch, push, or replay; a matching graph without these changes is
unchanged, and an unresolved, exhausted, or errored heal is failed.
Failed operations set status `ERROR`
without exporting raw error prose. There are no per-POST or per-question
spans and no in-process sampling.
Dropping an unfinished round or heal, including a dialer aborted after target
removal, finalizes it as `cancelled` with unset status, not `ERROR`. Explicit
completion and Drop consume the same recording, so counters and durations
are recorded once; cancellation does not itself increment the error counter.

| Instrument | Type | Unit | Complete attribute set |
| --- | --- | --- | --- |
| `st.replication.rounds` | Counter | `{round}` | `peer`, `outcome=moved|in_sync|failed|cancelled` |
| `st.replication.round.duration` | Histogram | `s` | `peer`, `outcome=moved|in_sync|failed|cancelled` |
| `st.replication.batch.size` | Histogram | `{envelope}` | `peer`, `direction=pull|push` |
| `st.replication.errors` | Counter | `{error}` | `peer`, `reason=down|overloaded|auth_failed|refused|removed|timeout|invalid|other` |
| `st.replication.heals` | Counter | `{heal}` | `peer`, `outcome=repaired|unchanged|failed|cancelled` |
| `st.replication.heal.duration` | Histogram | `s` | `peer`, `outcome=repaired|unchanged|failed|cancelled` |

Durations cover the whole call, including failed and cancelled calls, and use the seconds
buckets above. Pull batch sizes are locally stored envelopes; push sizes are
successfully sent envelopes. Neither counts signatures or claims.
`peer` is bounded by fleet members plus explicitly
configured/bootstrap peers; it is a node name, never a URL, route, session,
agent, or message id. Outcomes, directions, and reasons are closed
lowercase registries owned by Smalltalk. No other identities become metric
attributes.

Error reasons preserve worker failure classification: typed `PeerOverloaded`
maps to `overloaded`, `RemovedFromFleet` to `removed`, and
`FabricGrantRefusal` to `refused`. Reqwest timeouts map to `timeout`, other
reqwest failures to `down`, and JSON decoding, API-version mismatch, and
inflate failures to `invalid`. The legacy dialer's signature/fleet/member
authentication classification maps to `auth_failed`; unclassified errors map
to `other`, while recognized legacy failure-kind strings retain their mappings.
A signed HTTP refusal does not alone imply `refused`. A terminal unresolved or exhausted heal with
no classified cause uses `other`. Reasons never include error text.

Trace and metric enabled gates are independent: metrics record without a
trace exporter, and traces record without a meter provider. Disabled paths
return before constructing spans, instruments, attribute collections, or
telemetry-only timers/counts. An enabled operation owns one refcounted peer
label shared between its span and metrics; only metrics require a monotonic
start time. Batch/outcome/reason labels use static strings, and point
recording shares the peer label without copying its bytes.
Telemetry handles bind to the first provider in the process; production initializes once per process.
Enabled paths reuse existing exchange, receipt, and heal data rather than
querying the daemon for telemetry. `ReplicationReceipt` has no timing fields;
cumulative daemon `ReplicationTimings` are not fetched or turned into per-round
timings. No new IPC or derived timing math is added.

There are no replication backlog or lag gauges: the worker has no
`peer_only`/`local_only` counts and the store belongs to the daemon. This
surface adds no store scans or queries to manufacture those observations.

### Attribute and context policy

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

The wire-only tracestate entry `st=c` is owned by Smalltalk and means sampling is decided
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
peer context belongs inside the `FleetAuth`-signed header set. Other instrumentation
surfaces are recorded in [open questions](open-questions.md#st3).

### Proof and overhead

The core receiver proof uses `otelite` to inspect trace, metric, and correlated log export,
process identity and version, and the unset-endpoint no-export control. Trace proofs cover
export of fast roots and spans with unsampled remote parents; metrics record independently.
The daemon request proof checks caller trace continuity, `service.name=st-daemon`, and
`st.parent.sampled=true`/`false` for sampled and unsampled remote parents respectively,
the single-span shape (phase attributes present, no admission/handler child spans), and
that a healthy request exports no below-WARN log record. A unit test pins the batch queue
bounds.
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

Copied-store measurements compare endpoint-unset execution with an enabled `otelite` sink.
They cover daemon CPU, p99 request latency, RSS, and collector-sampled export rate against O11Y-R18.
The server request span shape is specified above. The core does not claim client/peer round-trip
coverage until those instrumentation surfaces exist.

### Design questions

The review questions and their resolution criteria are
[ST3-O11Y-DQ01–DQ05 and ST3-O11Y-DQ6](open-questions.md#st3): service naming, VRS placement,
signed peer context, sampling location, profiler ownership, and SIGTERM flush.

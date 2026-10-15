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

## st observability

The current daemon, CLI, replication worker and hook-exporter reference is
[st observability](../../st3/observability.md).

# Observability requirements

st2 and st3 emit OpenTelemetry signals about their own supervision work. This tree defines what
their telemetry must do and how it is proven. It follows the root [vision](../vision.md) and refines the
supervision subjects of the root [requirements](../requirements.md). It does not define fleet-wide
naming, provenance, or pipeline semantics — those are owned centrally by the dotfiles context
`observability` tree (`01-conventions` for naming/provenance/span-label rules, `09-integration`
for producer obligations, `otel-stack.md` for the LGTMP pipeline), which this tree references.

## Context

Until PR1, st2 had zero telemetry: no `tracing`, logging, or opentelemetry dependencies in
`Cargo.toml`, and diagnostics were bare `println!`/`eprintln!`. PR1 introduces trace export
([specification](spec.md)); metrics (PR2) and the log bridge (PR3) are still open. Durable
records — events, the sent ledger,
harness-state ([05-harness-state](../05-harness-state/)) — capture *what happened* but not *how
long it took*, *how often*, or *in what order across processes*. A supervisor that wedges in a
reconcile pass or a hung provider-session probe is invisible until a human reads a log file.

The fleet already runs an OTLP pipeline: producers ship OTLP/HTTP JSON to a per-host Alloy
forwarder at `127.0.0.1:4318`, which forwards to dev3 LGTMP and Grafana/gcx. st2 joins that
pipeline as one more producer; it does not invent its own.

## Requirements

### st2

- **O11Y-R01 Three signals:** st2 produces traces, metrics, and logs through OpenTelemetry.
  Traces cover the supervision control flow (roots listed in the
  [specification](spec.md)); metrics cover rates and durations of recurring passes;
  logs replace ad-hoc diagnostics on the paths where correlation matters. All three is the target,
  not traces alone.
- **O11Y-R02 No-op when unset:** Signals are emitted only when `OTEL_EXPORTER_OTLP_ENDPOINT` is
  set. When unset, telemetry is a zero-overhead no-op: no exporter threads, no network calls, no
  measurable cost on hot loops. Ambient configuration is honored automatically by exporter
  resolution. No proprietary st2 configuration surface exists beyond standard `OTEL_*` variables.
- **O11Y-R03 CI-proven:** The done-condition is proven in CI, not asserted. Integration tests run
  st2 against an `otelite` capture receiver and assert emitted spans/signals via its inspect
  mode. A build whose telemetry regresses to silence fails CI.
- **O11Y-R04 Provenance:** Every exported signal carries the fleet resource-attribute set:
  `service.name`, `service.namespace`, `service.instance.id`, `host.name`, `sk.site`, `sk.role`,
  and `deployment.environment.name`. Registered names are defined st2-side (this tree), not
  borrowed. `service.version` derives from the build stamp (`src/version.rs` reading
  `CLI_BUILD_STAMP`), the same identity the fleet `cli-version` shape carries.
- **O11Y-R05 Service naming by process unit:** `service.name` names the st2 process unit, not the
  repo — e.g. `st2-supervisor`, `st2-cli`, `st2-hook` — so a Grafana query groups a supervisor's
  lifetime separately from one-shot CLI invocations, per the central `01-conventions` rules.
- **O11Y-R06 Unit environment propagation:** The systemd supervisor unit propagates the
  operator's `OTEL_*` environment into the service: `src/service.rs` serializes the `OTEL_*`
  variables present in the launching environment into `Environment=` lines alongside the
  existing `PATH`/`PTY_ROOT` serialization, so `st2 up --install-unit` preserves ambient
  telemetry configuration (R02) under systemd.
- **O11Y-R07 Sync process model (st2 only):** Telemetry must not require an async runtime. st2's
  process model is synchronous (no tokio reactor); the exporter path must work under blocking clients.
- **O11Y-R08 Conformance posture:** Fleet-integration obligations are met as far as the st2 side
  allows: resource attributes (R04), naming (R05), OTLP endpoint via ambient env (R02). The
  remaining central obligations — the `telemetry.contract.ts` registry entry, the Grafana
  dashboard, and coverage-census subject registration — live in dotfiles' central observability
  tree and are explicitly deferred as cross-repo follow-up work, not part of st2's delivery.
- **O11Y-R09 Native-driver diagnostics:** Native-driver diagnostic
  failure/recovery transitions emit a bounded span/event and counter. The only
  metric-label axes are closed stage, reason, source, support, and outcome
  vocabularies. `span.label` is the bounded stage. Raw producer versions and
  agent/runtime/session/message identity are forbidden from metrics and
  `span.label`; raw prompt, message, and path content is forbidden from every
  diagnostic signal.

### st3

- **O11Y-R10 Process signals:** The st3 daemon, replication worker, and CLI use the OpenTelemetry
  SDK to produce traces, metrics, and logs. Each process unit has a distinct `service.name`.
  Hooks send signals through the daemon and never export directly. Harness-timer statusline
  commands remain exempt from telemetry initialization.
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
- **O11Y-R16 Sampling:** Within bounded trace buffers, st3 keeps all traces with an error or a
  local root longer than 1 second, plus a deterministic 1% of the rest. A sampled incoming
  parent also keeps the trace, subject to a limit of 20 parent-forced keeps per second per
  daemon. Buffer overflow drops are counted. Sampling does not affect metrics.
- **O11Y-R17 Bounded shutdown:** Telemetry flush and shutdown take at most 250 milliseconds
  for a CLI process and at most 5 seconds for a daemon or replication worker, including when
  the collector is unreachable.
- **O11Y-R18 Overhead budget:** With export enabled at fleet load (4–5 requests per second and
  0.7 reconcile passes per second), telemetry adds at most 2% daemon CPU, 5% p99 request
  latency, and 32 MiB RSS. Sampled export traffic is at most 50 KB per second per host.

The [specification](spec.md) owns the crate stack, exporter configuration, trace roots, and PR
stack. Open items are tracked in [open-questions](open-questions.md).

# Observability requirements

st2 emits OpenTelemetry signals about its own supervision work. This tree defines what st2's
telemetry must do and how it is proven. It follows the root [vision](../vision.md) and refines the
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
- **O11Y-R07 Sync process model:** Telemetry must not require an async runtime. st2's process
  model is synchronous (no tokio reactor); the exporter path must work under blocking clients.
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

The [specification](spec.md) owns the crate stack, exporter configuration, trace roots, and PR
stack. Open items are tracked in [open-questions](open-questions.md).

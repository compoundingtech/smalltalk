# Observability open questions

Kept minimal; each blocks exactly one delivery slice, not the tree.

- **PR2 metric set.** Landed: the RED-minimal set from interview decision Q5, with names,
  types, and label enums as specified in [spec.md](spec.md) (`src/metrics.rs`); the
  `st2 up <spec>` span coverage folded in as planned — all three reconcile-pass sites now
  emit `st2.reconcile_pass` plus the pass counter and duration histogram.
- **Remaining R04 resource attributes.** Resolved by source read (dotfiles dev3
  `monitoring.nix` transform block): the platform edge stamps `service.namespace`,
  `sk.site`, `sk.role`, `deployment.environment.name` where absent, and the central
  contract forbids hand-stamping them producer-side. st2 keeps `service.name`,
  `service.version`, `host.name`; nothing left to wire.
- **PR3 log bridge approach.** Resolved by interview (decision record Q6) and LANDED: the
  `tracing` facade (`tracing-opentelemetry` + `opentelemetry-appender-tracing`) unifies spans
  and logs on one subscriber; emit sites migrated to tracing macros; the unset-endpoint case
  keeps the stderr fmt layer so diagnostics stay visible (documented deviation from PR1's
  zero-output reading). Larger diff accepted for the long-term win; PR1's dual-path helper is
  not built.
- **Unit env mechanism.** Resolved at PR1 implementation time: `Environment=` lines,
  captured at install time and unit-tested (`src/service.rs`); matches the existing
  PATH/PTY_ROOT pattern and the expected handful of variables. Revisit
  `EnvironmentFile=` only if the variable count grows.
- **Sampling.** Default is always-on given st2's low event volume. If supervisor-loop span volume
  proves noisy in Grafana, revisit parent-based sampling ratios — not before there is data.

## st3

These review questions come from [#1580](https://github.com/compoundingtech/smalltalk/issues/1580)
and link to the [st3 design questions](spec.md#design-questions).

- **ST3-O11Y-DQ01 Service names:** Are process names `st3-*` or version-neutral `st-*`?
  Resolve by approving the registered service-name vocabulary; the core selects `st3-*`.
- **ST3-O11Y-DQ02 VRS placement:** Does st3 belong in this observability tree or a sibling
  tree? Resolve by approving the document boundary and the st2-only scope of O11Y-R07;
  the core uses a separate st3 section here.
- **ST3-O11Y-DQ03 Signed peer context:** Is `traceparent` inside the `FleetAuth`-signed
  header set acceptable? Resolve by reviewing the signing contract and old-peer behavior.
- **ST3-O11Y-DQ04 Sampling location:** Does local-root tail sampling run in process, or
  does a central collector sample all exported spans? Resolve by reviewing network cost,
  buffer bounds, and missing fast downstream groups; the core selects in-process sampling.
- **ST3-O11Y-DQ05 Profiler ownership:** Does `ST3_PROFILE_DIR` remain a separate artifact,
  or export the same spans? Resolve by comparing profiler coverage, overhead, and span
  vocabulary with the SDK path.

The core does not yet specify emission-site mechanisms for server request/admission/handler
and stream spans, writer/read-pool spans, reconcile/FIFO/WAL metrics, startup phase spans,
replication exchange/lag instruments, raw-terminal spans, client context injection, signed
peer context, hook environment context, or durable-boundary span links. Their concrete
mechanisms must preserve O11Y-R10–R18; this list is a scope boundary, not a delivery order.

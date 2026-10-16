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

## st observability

The current mechanism and remaining integration boundaries are in
[st observability](../../st3/observability.md#integration-boundaries).

# st3 documentation

The root [README](../../README.md) explains the product and the normal command workflow.

Use these documents for implementation details:

- [Architecture](design.md) defines the stable system shape.
- [Mission graph runtime](mission-graph-runtime.md) defines the KDL language and execution model.
- [KDL lifecycle](kdl-lifecycle.md) defines day-to-day publication and revision workflows.
- [Schema registry](schema.md) lists the generated public subject, resource, and claim vocabulary.
- [Data authority](data-authority.md) separates durable facts from projections and caches.
- [Fleet replication](replication.md) defines convergence, inspection, and repair.
- [Resource subscriptions](resource-subscriptions.md) defines observers and automatic intake.
- [Agent migration](agent-migration.md) defines isolated rehearsal, cutover, and rollback.
- [Eval audit](eval-audit.md) records the test intent and prompt boundary for each st3 eval.
- [Product roadmap](roadmap.md) records accepted future work.
- [Guided CLI tour](cli-guided-tour.md) is the complete human walkthrough for every public command
  and subcommand.
- [TUI and Expo iOS design-session brief](app-design-session-brief.md) captures the product promise,
  delivery gates, remote transport research, and autonomous release loop to review before UI work.
- [Client v0 contract](client-v0/README.md) defines the shared TUI and mobile JSON, event, action,
  pairing, and terminal protocols.
- [Operational-state contract](operational-state/README.md) separates immutable history, current
  projection, and actor-specific actionable views and defines screen/CLI parity.

The [examples](../../examples/st3/README.md) show small, tested mission patterns. Use the evals for
failure proof, not as introductory examples.

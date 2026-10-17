# Versioned convergence inputs

`claims-v1.json` is the fixed pretty-printed JSON serialization of
`st3_schema::registry().claims` from the retirement candidate based on
`92c17ce0fe163cacbe907179cbafd7a98782f78d`. It includes `agent.retired` and its
reserved fields and remains unchanged for reproducible comparisons.

`claims-v2.json` is generated from the review-reader registry at
`5e9b1014226fca3aded0321eccc08279cf9ffded` (schema digest
`c84c80daf817aef1c390282883197a79c10604ce76094a7ac5da84998cae7270`).
Its generation expression is
`serde_json::to_string_pretty(&st3_schema::registry().claims)`, followed by a
newline. It adds the four durable review claim kinds and captures the complete
review request field schema. The simulation includes this version and records
its filename in audit reports.

The seeded generator draws kinds and fields from its selected snapshot; each
tested revision still admits them through its own native registry and store.
The corpus conformance test requires every live registry kind to be represented.

`claims-v3.json` is the serialization of the registry from harness-question candidate
`7935729a8e564c178ac9b83e940d87dcc543c9f2` (registry digest
`2d3e67b713e9c515b0672761daebbadfe29cc4dc43f6721ed995dd8a2199daa7`).
It adds `harness-question.observed` and `harness-question.resolved`; v1 remains
unchanged. Version 2 belongs to main's review-reader registry; version 3 is the next free
corpus slot. After merging main, regenerate v3 with
`cargo run -p st3-schema --example claims_json -- fixtures/convergence/claims-v3.json`.
The generator serializes the complete merged registry; do not hand-edit the snapshot.

Keep the snapshot fixed when comparing revisions. For a future schema change,
create a new version with `serde_json::to_string_pretty(&st3_schema::registry().claims)`
and deliberately update the simulation's include. Apply identical simulation
support and the same snapshot to the baseline; do not copy production admission
or retirement changes into it. The existing checkpoint and planning-session
generator exclusions remain in the generator, where their reasons are documented.

The simulation also scopes checkpoint build/executable identity to fixed values.
Actual production proof identity is unchanged. A run's clock and random generator
come from its seed, rather than host time or the executable hash.

Run the opt-in wide audit with `cargo test --release -p st3 --features
convergence-audit --test convergence_audit -- --ignored --exact
convergence::fixed_sweep --nocapture`. Set `CONVERGENCE_REPORT_DIR` to a fresh
directory, `CONVERGENCE_START` (default 1), `CONVERGENCE_RUNS` (default 500), and
`CONVERGENCE_WORKERS` (default 4). Set `CONVERGENCE_SAVE_FAILURE=1` to preserve
failed worlds and their report. Use the host's configured test runner when
executing a copied binary.

The audit records every seed and fails after collecting all outcomes. Reusing the
same output directory resumes completed outcomes, including failures; it is only
valid with the same input snapshot, simulation source and native revision.
Keep the source and binary hashes with the reports. Never discard a failed
outcome when resuming an interrupted audit.

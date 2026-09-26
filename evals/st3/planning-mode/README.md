# Planning Mode

This paid st3 eval proves the first planning-mode workflow with one native Codex planner.

The controller starts a durable launch. It waits on the internal authoring event stream until the planner submits Markdown and KDL documents. It then renders the static graph and graph diff and directly approves the exact preview token.

Mechanical gates prove these boundaries:

- No mission is published before approval.
- The preview shows the explicit `inspect` to `verify` dependency.
- Approval publishes exactly one ready mission.
- Approval does not start a mission run.
- The published mission links the immutable Markdown and KDL documents.
- The planner stops after approval.
- The planning workspace does not change.

The revision path and stale-preview refusal are deterministic API tests. This paid eval uses direct approval so the model budget measures authoring, not a forced rewrite.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.

# Source eval migration review

Date: 2026-08-28

Source: `compoundingtech/evals` at `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.

This review covers all 58 active source evals. It also considers the four evals that the source already retired.

The source repository calls these units cells. This repository calls each unit an eval.

## Recommendation

Archive the source repository after the selected evals and evidence enter this repository.

Do not copy every eval as KDL. Product behavior with strong Rust coverage belongs in the Rust suite.

Use one logical eval for one behavior. A Claude or Codex variant is a harness choice, not a new eval.

The final result has 19 logical evals. All 19 logical evals now have both runtime forms.

- The review started with three logical evals represented here.
- Sixteen new logical evals come from 18 source evals.
- Three source evals merge into retained evals.
- Twelve source evals remain as Rust coverage.
- Nineteen research, overlapping, or rejected-control evals do not enter the active corpus.

The source catalog lists accepted pass evidence for seven evals. Most evals have no structured run record.

Selection therefore depends on the product contract. It does not depend only on a historic green result.

The final corpus has ten model-free evals and nine paid evals. The nine paid evals include the three original evals.

## Paid eval admission rule

A paid eval must expose a unique product failure. A cheaper Rust or model-free test must not cover the same failure.

The eval must have a strong held-out judge. A vague model-quality score does not earn a place in the active corpus.

The eval must also have a named run trigger. We do not run every paid eval after every change.

A candidate remains outside the active corpus until one baseline run proves that its judge works.

## Decision terms

`Represented` means that the behavior already has an st2 and an st3 form in this repository.

`Keep` means that the fixture enters this repository with rewritten KDL for the current design.

`Merge` means that another logical eval should absorb the unique assertions.

`Rust` means that we should not keep a second KDL copy. The named Rust tests own the contract.

`Archive` means that the source Git history remains the record. The active corpus does not copy the eval.

## Complete classification

| Source eval | Decision | Target or owner | Reason |
| --- | --- | --- | --- |
| `adopt-only-migration` | Rust | `tests/reconcile.rs`, `tests/run.rs` | The Rust suite covers live adoption, absent hold, dead hold, and explicit replacement. |
| `agent-spec-resource-bindings` | Rust | `crates/agent-spec/tests/discovery.rs`, `tests/validate.rs`, `tests/reconcile.rs` | The Rust suite covers parsing, stable projection, and no-restart resource edits. |
| `assignment-contract-cold-assignment` | Archive | Source history | This old Assignment wrapper was an experimental control. It is not an st3 mission assignment. |
| `assignment-contract-cold-focus` | Archive | Source history | Focus was an experimental selector control. The source project did not select it. |
| `assignment-contract-cold-resources` | Represented | Model-free `resource-cold-start` | Test resource readiness and one delivery action without paying a model. |
| `assignment-contract-handoff-assignment` | Archive | Source history | This old Assignment wrapper was an experimental control. It is not an st3 mission assignment. |
| `assignment-contract-handoff-focus` | Archive | Source history | Focus was an experimental selector control. The source project did not select it. |
| `assignment-contract-handoff-resources` | Represented | Model-free `resource-handoff` | Test revoke-before-grant handoff without paying a model. |
| `assignment-contract-hot-assignment` | Archive | Source history | This old Assignment wrapper was an experimental control. It is not an st3 mission assignment. |
| `assignment-contract-hot-focus` | Archive | Source history | Focus was an experimental selector control. The source project did not select it. |
| `assignment-contract-hot-resources` | Represented | Model-free `resource-retarget` | Test retarget, removal, and idle graph states without paying a model. |
| `context-resource-continuity` | Represented | `context-resource-continuity` | This fast eval crosses the real lifecycle boundary and checks durable state together. |
| `crash-ding` | Represented | Model-free `crash-escalation` | Crash classification and escalation are mechanical runtime behavior. |
| `ding-mode` | Merge | `license-mit` | License MIT already covers delegated work over Small Talk. Keep only missing message assertions. |
| `ding-reply` | Merge | `license-mit` | Add a threaded-reply assertion to License MIT. A separate paid agent does not earn its cost. |
| `docs` | Archive | Source history | The cold-reader idea is good, but three paid seats test model writing more than runtime behavior. |
| `exec-lifecycle-logging` | Rust | `tests/exec_backend.rs` | The Rust suite covers process lifetime, logs, rotation, adoption, and group teardown. |
| `feature-fit` | Archive | Source history | Ghost bug and Signal rename already cover work in an existing codebase. |
| `fork-in-the-road` | Represented | `fork-in-the-road` | The local Codex pair tests parallel analysis, debate, synthesis, and a real tradeoff. |
| `fork-in-the-road-codex` | Represented | `fork-in-the-road` | The local Codex pair represents both source provider forms. |
| `ghost-bug` | Represented | `ghost-bug` | The local Codex pair represents both source provider forms. |
| `ghost-bug-codex` | Represented | `ghost-bug` | The local Codex pair represents both source provider forms. |
| `hook-integrity` | Rust | `tests/hooks.rs`, `tests/materialize.rs`, `tests/claude_hooks.rs`, `tests/codex_hooks.rs` | The Rust suite covers receipts, verification, native overlays, and idempotency. |
| `host-lock-health-negatives` | Rust | `tests/doctor.rs` | The Rust suite covers required, stale, foreign, and bounded-failure lock states. |
| `inbox-hygiene` | Represented | `restart-continuity` | The restart eval injects duplicate delivery. One durability eval covers both failures. |
| `incident-response` | Archive | Source history | Its core root-cause workflow overlaps Ghost bug and costs two seats. |
| `license-mit` | Represented | `license-mit` | The local Claude pair represents both source provider forms. |
| `license-mit-codex` | Represented | `license-mit` | The local Claude pair represents both source provider forms. |
| `migration` | Archive | Source history | Its broad code migration overlaps the stronger Signal rename eval. |
| `poisoned-pr` | Represented | `poisoned-pr` | The local Codex pair gives the corpus a strong review-only lane. |
| `poisoned-pr-codex` | Represented | `poisoned-pr` | The local Codex pair represents both source provider forms. |
| `presence-ding-matrix` | Rust | `src/ding/mod.rs` tests | The Rust suite covers busy, away, DND, stale DND, FIFO, and retry behavior. |
| `pty-attach-machine-stream` | Represented | `pty-attach-machine-stream` | This verifies the installed PTY stream through the boundary that st2 and st3 use. |
| `pty-attach-only` | Represented | `pty-attach-only` | This verifies that attach never creates or restarts a session. |
| `pty-send-peek` | Represented | `pty-send-peek` | This gives a real byte round trip and has accepted pass evidence. |
| `reconcile-retire-keep` | Rust | `tests/reconcile.rs` | The Rust suite covers missing, dead, keep, retired, and adopt lifecycle rules. |
| `render-target-safety` | Rust | `tests/materialize.rs` | The Rust suite covers directives, idempotency, tracked targets, and unsafe paths. |
| `restart-continuity` | Represented | `restart-continuity` | Both runtime forms test cold restart, duplicate delivery, and one result per stable item ID. |
| `security-audit` | Archive | Source history | Poisoned PR gives a sharper security discriminator with the same two-seat cost. |
| `shared-workspace-render-ownership` | Rust | `tests/materialize.rs`, `tests/validate.rs` | The Rust suite covers conflicting ownership and identical shared claims. |
| `signal-rename` | Represented | `signal-rename` | The local Codex pair represents both source provider forms. |
| `signal-rename-codex` | Represented | `signal-rename` | The local Codex pair represents both source provider forms. |
| `skill-inheritance` | Represented | `claude-skill-inheritance` | This is a useful Claude package and project-skill integration eval. |
| `st2-doctor-structure` | Rust | `tests/doctor.rs` | The Rust suite checks healthy and broken catalogs with mutation-valid failures. |
| `st2-network` | Represented | Model-free `network-smoke` | Keep one black-box proof that hosting and message delivery work together. |
| `strict-validation-json` | Rust | `tests/validate.rs` | The Rust suite covers stable issue data and strict warning promotion. |
| `targeted-reconcile-isolation` | Rust | `tests/targeted_reconcile.rs` | The Rust suite covers exact selection and sibling isolation. |
| `test-writing` | Represented | `test-writing` | Mutation scoring makes this stronger than a test-count eval. |
| `two-networks-coexist` | Represented | Model-free `network-isolation` | Keep the concurrent catalog, message, and PTY partition proof. |
| `vrs-cross-file-absent` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-cross-file-present` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-definition-of-done-absent` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-definition-of-done-present` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-scope-drift-absent` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-scope-drift-present` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-scope-pressure-absent` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `vrs-scope-pressure-present` | Archive | Source research history | This matched pair tests a governance document treatment, not runtime behavior. |
| `weird-git-setup` | Represented | `weird-git-setup` | This is a real launch environment edge case and has accepted pass evidence. |

## Why the selected resource evals survive

The nine assignment evals form a three-by-three experiment. The source project selected direct named resources.

The Focus and old Assignment forms remain controls. They should not become new st3 product layers.

This decision does not remove mission assignment from st3. An st3 mission step still has an explicit agent assignment.

The new model-free evals should test three different events:

- A cold assignment becomes ready and creates one delivery action.
- A live assignment receives a new resource target and later becomes idle.
- One subject loses authority before another subject receives authority.

The evals should inspect missions, resources, claims, and Small Talk actions directly. They should not pay models to confirm mechanical state.

## Why the VRS pairs do not enter the active corpus

The eight VRS evals form four matched prompt experiments. They test the effect of governance documents on model behavior.

That research can remain useful. It is not a runtime acceptance contract, and none of the pairs has structured run evidence.

The archived repository preserves the fixtures and judges at the pinned commit. We can later create a separate research corpus.

We can also reuse an individual task after we define a clear runtime question. We should not carry the A/B structure by default.

## Migration rules

Each selected agent eval must use a native `harness` block. A fixture process can still use `command`.

Each selected logical eval gets an st2 form and an st3 form.

A model-free pair uses native runtime records to prove the same outcome. Its report names all tested binary versions.

The st3 form must lift the work mission, step state, assignment, products, and judges into the graph.

The graph must hold durable work state. Agent prose and a private todo file must not be the only state.

The combined restart eval distinguishes at-least-once execution from one accepted product for each stable work ID.

The st3 form must use Small Talk for direct coordination. It must not add a private message channel.

Codex `gpt-5.6-sol` is the default model judge. A specific eval can choose a Claude judge and record the reason.

Do not add an old-KDL adapter. Translate a selected st2 KDL before st3 publishes it.

Every paid run must add a dated report. The report records duration, token use, graph transitions, messages, and judge results.

## Migration order

Wave 1 proves the runtime:

1. Maintain the ten represented model-free lifecycle, PTY, network, resource, and crash pairs.
2. Add threaded-reply and delegation assertions to License MIT.
3. Maintain Restart continuity with duplicate delivery and durable graph state.
4. Maintain Fork in the road with its parallel drafts, debate, revisions, and synthesis in the graph.

Wave 2 broadens agent work:

1. Maintain Poisoned pull request as the review-only and security eval.
2. Maintain Test Writing as the mutation-scored quality eval.
3. Maintain Weird Git Setup as the worktree launch eval.
4. Maintain Claude Skill Inheritance as a provider integration eval.

Rust-owned evals do not wait for these waves. Before source archival, run their named suites and close any missing assertion.

## Paid run triggers

| Eval | Unique paid value | Run trigger |
| --- | --- | --- |
| `license-mit` | Claude team startup, delegation, threaded Small Talk, and confirmation | Claude harness, Small Talk, or delegation changes |
| `ghost-bug` | The cheapest Codex multi-agent debugging path | Routine Codex team smoke testing |
| `signal-rename` | A complex mission with parallel code work, products, integration, and a green review state | Mission graph, dependency, product, or multi-agent changes |
| `fork-in-the-road` | Parallel independent analysis followed by synthesis of a real tradeoff | Concurrency and synthesis milestones only |
| `restart-continuity` | Cold restart, duplicate delivery, and durable graph progress in one scenario | Restart, redelivery, lease, or durable work-state changes |
| `poisoned-pr` | A review-only lane with a hard security finding and request-changes verdict | Review judgement or review-only lane changes |
| `test-writing` | Mutation scoring detects coverage theater | Judge execution, fixture isolation, or result-report changes |
| `weird-git-setup` | A real linked-worktree launch and commit boundary | Workspace, worktree, or launch-path changes |
| `claude-skill-inheritance` | Actual project and plugin skill loading in the Claude harness | Claude package, plugin, or project-skill changes |

## Archive gate

The source repository can become read-only when all items below are true:

- Each `Keep` row exists here or has an explicit removal decision.
- Each `Rust` row has green named coverage on supported platforms.
- Imported source evidence includes its source commit and original run receipts.
- The local inventory generates from the embedded corpus.
- The old overnight runner has no unique production duty.
- The source repository README points to this repository and its exact corpus location.
- One final script proves that all 58 active source evals have one decision in this review.

The four source-retired evals remain retired: `clean-compose`, `compose-config-load`, `compose-global-skill`, and `team-standup`.

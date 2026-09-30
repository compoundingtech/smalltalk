# CI operations

## GitHub Actions on Namespace

The generated `Workspace CI` workflow (`.github/workflows/fleet.yml`) and `macOS CI`
(`.github/workflows/macos.yml`) replace the fleet's Linux `st/ci` and optional `st/ci-macos`
execution. During the proving period both systems
run in parallel, and the **live** merge rule remains `st/ci` until the coordinated switch below.
The proposed required check names are `linux-gate` and `genie-freshness`.

Every pull request, including a fork and a draft, gets the Linux gate and freshness check.
Checkout uses GitHub's default `pull_request` merge ref, not the contributor's unmerged
head: it tests that head merged with the current base. Strict branch protection also requires
that the head itself contain the latest `main`. No `pull_request_target` job runs PR code,
and the gate has only `contents: read` permission. Forks do not receive publishing secrets.

Linux uses `namespace-profile-linux-x86-64`. The gate prepares the provider component
fixtures and installs matching rendered st2 hooks, then builds the workspace test executables
first, alone, with dev/test debug info and incremental compilation disabled. After that build,
`scripts/ci-linux` starts four stages in parallel and fails if any stage fails:

- `cargo nextest run --workspace --locked --profile ci`, eight tests at a time;
- `cargo clippy --workspace --all-targets --locked`;
- `cargo run --locked -p st3-client-codegen -- --check`;
- the fleet compatibility test against `.github/fleet-compat-baseline.json`'s pinned older st3.

`.config/nextest.toml` gives the messaging fault matrix and the fleet reconnect test, both with
real multi-minute outages, first priority so their retries fit the CI test window. Failed tests
retry twice with fixed 30-second delays; a retry pass is reported as flaky, not a gate failure.
Clippy uses a separate target directory so its Cargo lock cannot serialize the parallel stages.
Each run isolates test `HOME` and XDG state. The summary records the tested SHA, test selection,
each stage's elapsed time, result and exit code; `linux-ci-logs` contains logs and `.time` files.
Nextest's final summary retains flaky outcomes.

The workspace suite still covers the token-free two-node messaging fault matrix. Its historical
channel build remains independently pinned in `.github/messaging-compat-baseline.json`.
See [the eval contract](../evals/st3/messaging-faults/README.md).

### st2 catalog/supervisor selection

`scripts/ci-st2-filter` uses the complete PR merge-base diff, retaining both deleted and added
paths for renames. It skips st2's unrelated catalog/supervisor tests only when **every** changed
path is st3 code, clients, documents, one of the st2 driver/channel/hook/message/harness-state/
session modules st3 uses, or another individual st2 test file. The excluded st2 module families
are `agent_author`, `catalog*`, `eval_run`, `resync` and `resource_profile_supervisor`; excluded
test families are `catalog_*`, `nomad_survival`, `event_e2e`, `eval_run_e2e`, `resync*`,
`supervisor_auto_archive` and `resource_profile_supervisor_e2e`.

An unknown or shared path enables the complete suite, including `Cargo.toml`, `Cargo.lock`,
shared crates, `tests/support/` and the integration-test module manifest. A summary says
"st2 catalog and supervisor tests not needed" when they are skipped. Main pushes run the
st3/common suite; the daily 04:23 UTC schedule runs the complete workspace on `main`.
Manual workflow dispatch also runs the complete suite. This replaces the mission's historical
"first main run after a day without a passing complete run" policy with an explicit daily lane.
Run `python3 scripts/ci-st2-filter-test` to check the selection boundaries.

### macOS

The non-required `macos-ci` job uses `namespace-profile-macos-arm64` and runs on `main` pushes
or PR events while the PR bears the `macos-ci` label. It is a separate workflow so adding a
label does not restart or cancel the required Linux gate; it therefore runs beside Linux rather
than waiting for Linux success. It builds the same workspace without debug info/incremental
compilation and runs nextest with a 25-minute test-step timeout, followed by Clippy. The
Codex control tests are not filtered out. Namespace provides job-isolated runners rather than
reusing the fleet's long-lived target lanes and macOS debug-object cleanup policy.

The Namespace GitHub App must be installed and authorized for this repository. Its installation
has not been confirmed; jobs can queue indefinitely with no matching runner. Do not silently
fall back to hosted or fleet runners.

## Generated files and existing workflows

All workflow YAML and `.github/repo-settings.json` are generated from neighboring `.genie.ts`
files. The `effect-utils` flake input supplies the generator library and CLI. The small, separate
`genie` shell creates `repos/effect-utils` as a symlink to the exact input's Nix store path;
`repos/` is ignored. No local `node_modules` or megarepo adoption is needed. The default Rust
shell is not changed to depend on genie, so generation does not build its PTY/collector tools.

```sh
nix develop .#genie -c genie
nix develop .#genie -c genie --check
nix flake check --no-build
```

`genie-freshness` runs the second command on Linux. Nix CI setup uses the public
`overeng-effect-utils` Cachix descriptor as a read-only substituter, with no publishing token.
The preview input also supplies `otelite`, so updating this pin changes the collector used by
release-integration and the default shell. Re-pin to effect-utils main once the Rust helper and
repo-settings changes have merged.

| Workflow | Change and reason |
| --- | --- |
| `fleet.yml` | The old fork-only fleet compatibility job is absorbed into `linux-gate`, which now covers every PR and main. This preserves fork coverage and adds same-repository coverage on Namespace. |
| `nix.yml` | Remove the fork-only nextest/Clippy job because the new Linux gate includes those checks and provider fixtures. Retain the tag-only full Nix release/check graph and its cache action. |
| `release-smalltalk.yml` | Preserve tag/dispatch/fork-PR triggers, target packaging, source verification and publishing permissions. Move Linux and ARM macOS runners to Namespace. |
| `release-portable.yml` | Preserve dispatch inputs, accepted-source verification, packaging, publishing and fresh-download execution proof. Move Linux to Namespace. |
| `public-repo.yml` | Preserve the guard and its tests on all PRs and main pushes; move Linux to Namespace. |

The content guard still rejects real machine/home identities and private fleet configuration
references. Release workflows remain separate from the required gate; neither package
verification nor the tag-only Nix graph is made redundant by workspace nextest.

## Merge rule and switch-over

The desired `main` ruleset requires `linux-gate` and `genie-freshness` from GitHub Actions,
`strict_required_status_checks_policy=true`, and an empty bypass list. It preserves the live
pull-request, deletion and force-push protections. Repository settings enable GitHub native
auto-merge and branch deletion after merge; these settings do not enable auto-merge on a PR.
The ruleset is **not applied automatically by CI**.

Nathan owns the branch updater and must approve the switch-over order before an administrator
applies settings. The train driver is outside this public repository; this change does not edit it.

1. Deploy the generated workflows while leaving `st/ci` required and its mission active. Prove
   `linux-gate` and `genie-freshness` green alongside `st/ci` on several PRs, including a real
   behind-main update and fork coverage. Confirm Namespace capacity and label-driven macOS.
2. Nathan changes the train to wait for the two GHA check runs instead of the `st/ci` commit
   status. The train still updates one branch at a time with latest `main`, retains lane approval
   and stale-head handling, and waits again after every update. It must require GitHub Actions
   check runs for the **current PR head SHA** to be completed with `success`; queued, missing,
   skipped, cancelled or stale-SHA results mean waiting. It re-reads head/base before merging or
   handing the car to GitHub native auto-merge. During this phase the still-active `st/ci` rule
   also remains enforced by GitHub.
3. Only after Nathan confirms the new train path and the proving runs, an administrator applies
   `.github/repo-settings.json` from the reviewed checkout. The PR contains the exact pinned
   `gh-apply-settings` command and the captured old ruleset JSON. Check the live required contexts
   immediately; do not remove `st/ci` before the train understands GHA, and do not retire its
   producer before removing it from the ruleset.
4. Retire the fleet CI mission, not the merge-train mission. Keep the lane as the branch updater.

Applying the checks out of order freezes merges. For rollback, restore/keep the old CI mission,
re-apply the old `main` ruleset JSON captured in the PR, then have Nathan restore its old
`st/ci` wait logic. Never retire the old producer before its required check has been removed.

## Merge train

The merge train remains a [lane](st3/lanes.md) named `smalltalk`. Join and inspect it as before:

```sh
st lanes join smalltalk NUMBER
st lanes show smalltalk
```

The train retains front-first branch updates and human approval policy. It merges `main` into
only the current car, waits for fresh GHA checks, and yields/reorders when a head changes, main
moves, or a gate fails. The train may merge itself or hand its current car to GitHub native
auto-merge; either path enforces the same ruleset.
A draft still cannot merge, even though GHA runs it. Fork code is covered by GHA; the existing
train's fork-membership policy is not changed by this repository patch.

To merge by hand, update the branch (`gh pr update-branch NUMBER`), wait for fresh required
checks, and merge only while the branch remains current. Pull before pushing after the train
updates your branch. A hand merge does not retire or disable the train.

## Inspect a failure

Use the PR Checks tab or `gh run view RUN_ID --log-failed`. The Linux job uploads `linux-ci-logs`
even on failure. Inspect each stage's log and timing, the selected suite and checked merge SHA.
A passing retry is a flaky outcome in the nextest log. A queued Namespace job with no runner
is infrastructure readiness, not a successful check; the train must keep waiting.

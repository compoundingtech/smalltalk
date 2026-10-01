# CI operations

## GitHub Actions on Namespace

Every push to `main` runs both `Workspace CI` and `macOS CI` on Namespace. Each non-PR run
uses its own `github.run_id` in the concurrency group, so successive pushes can run concurrently
without cancelling running checks or replacing pending runs. PR updates still cancel stale
checks for that PR; macOS checks on PRs require the `macos-ci` label.

The generated `Workspace CI` workflow (`.github/workflows/fleet.yml`) and `macOS CI`
(`.github/workflows/macos.yml`) replace the fleet's former Linux `st/ci` and optional `st/ci-macos`
execution. The required checks on `main` are `linux-gate`, `isolation-vm` and `genie-freshness`,
and `main` lands through GitHub's merge queue (see [Merge queue](#merge-queue)).

Every pull request, including a fork and a draft, gets the Linux gate, the isolation VM and the
freshness check. `Workspace CI` also runs on the `merge_group` event, so GitHub's merge queue receives
the three required checks for each queued entry.
Checkout uses GitHub's default `pull_request` merge ref, not the contributor's unmerged
head: it tests that head merged with the current base. Strict branch protection also requires
that the head itself contain the latest `main`. No `pull_request_target` job runs PR code,
and the gate has only `contents: read` permission. Forks do not receive publishing secrets.

The Linux gate runs as three jobs on separate runners, so they no longer share one machine's CPUs.
`linux-gate` is the single required check: it needs the three jobs and passes only when every one of
them succeeded (a skipped or cancelled stage fails it). The stage jobs use the Namespace shape
`nscloud-ubuntu-24.04-amd64-16x32` (16 vCPUs, 32 GB); `genie-freshness`, `isolation-vm` and the
`linux-gate` aggregate use `namespace-profile-linux-x86-64`. `scripts/ci-linux STAGE` runs one stage:

- `linux-tests`: prepares the provider component fixtures, installs matching rendered st2 hooks,
  builds the workspace test executables with dev/test debug info and incremental compilation
  disabled, then runs `cargo nextest run --workspace --locked --profile ci` on every CPU, selected
  by the profile's default filter (see [gate scope](#gate-scope));
- `linux-clippy`: `cargo clippy --workspace --all-targets --locked`, then
  `cargo run --locked -p st3-client-codegen -- --check`;
- `linux-fleet-compat`: the fleet compatibility test against `.github/fleet-compat-baseline.json`'s
  pinned older st3. Building that baseline also runs the pinned pty's own unit tests, two of which
  are timing-sensitive, so the build is retried up to three times.

Each stage restores a job-keyed `actions/cache` entry (Namespace serves it from its accelerated
backend) holding Cargo's registry and the workspace `target/` directory, keyed on `Cargo.lock` and
`flake.lock`. Namespace cache volumes are not used: they are per node and replicate in the
background, so a job landing on another node starts empty.

The messaging fault matrix runs as eleven independent `messaging_faults::*` tests in
`linux-tests`. Nextest schedules the cases in parallel and retries each failing case separately.
Each case keeps its own evidence directory under `target/messaging-faults/`. The fixture uses
a systemd user runtime only when its bus exists, so runners without a user manager use the
existing detached process path instead of trying to create scopes through a synthetic runtime.

`.config/nextest.toml` gives the messaging fault matrix and the fleet reconnect test, both with
real multi-minute outages, first priority so their retries fit the CI test window. Failed tests
retry twice with fixed 30-second delays; a retry pass is reported as flaky, not a gate failure.
Each run isolates test `HOME` and XDG state. The summary records the tested SHA,
each stage's elapsed time, result and exit code; each stage job uploads `<job>-logs` with its log and `.time` file.
Nextest's final summary retains flaky outcomes.

The workspace suite still covers the token-free two-node messaging fault matrix. Its historical
channel build remains independently pinned in `.github/messaging-compat-baseline.json`. Its
provider stand-in runs the omp channel hook's TypeScript with Node 24's built-in type stripping;
the default devShell supplies that `node`. See [the eval contract](../evals/st3/messaging-faults/README.md).

### Gate scope

The gate covers st3 and the code st3 uses. The `ci` profile's `default-filter` in
`.config/nextest.toml` selects it on every event, with no path filter or scheduled full run:

- every test of the other workspace crates: st3, st3-client, st3-client-codegen, st3-schema,
  st3-migrate, stui, st-runtime, st-drivers (the harness drivers, channels, hooks, messages,
  harness state and sessions st3 and st2 share) and the shared and resource-provider crates;
- st2's integration test files, apart from those below.

It leaves out st2-only tests: st2's own unit tests (none of its remaining modules is used by
st3), the st-drivers `catalog*`, `resync` and `resource_profile_supervisor` modules, and st2's
`catalog_*`, `nomad_survival`, `event_e2e`, `eval_run_e2e`, `resync*`, `supervisor_auto_archive`
and `resource_profile_supervisor_e2e` tests. No CI job runs these. Clippy still checks the whole
workspace. List the selection with `cargo nextest list --workspace --profile ci`.

### Isolation VM

`tests/transport_isolation.rs` proves that a task st2 starts in its own systemd user scope
survives a SIGKILL of its supervisor's cgroup, for both exec and pty tasks. The managed-agent
color contract also checks environment propagation through a real user scope, including a
PTY restart. These three tests need a real systemd user manager, which Namespace's runner
image does not boot. They run in a NixOS VM (`nix/transport-isolation-vm.nix`) in the required
`isolation-vm` job:

1. Probe `/dev/kvm`: the job fails unless KVM can create a VM. Namespace offers nested
   virtualization on `linux/amd64`. QEMU is configured with `forceAccel`, and the test checks
   `systemd-detect-virt` reports `kvm`, so it never falls back to emulation.
2. `cargo nextest archive -p st2 --test integration` builds the integration test binary and st2.
3. The job builds the VM test driver from the flake and runs it on the runner. The VM boots
   NixOS with a lingering user, copies in the archive, extracts it at the checkout's path (the
   test binary has st2's path compiled in) and runs both cascade tests and
   `nomad_survival::managed_agent_color_contract_crosses_systemd_scope` with nextest as a
   transient service of that user's systemd manager. The VM compiles nothing.

The VM requires all three tests to run and pass, with no isolation opt-out. The job summary
records the KVM probe and each phase's elapsed time.

### macOS

The non-required `macos-ci` job uses `namespace-profile-macos-arm64` and runs on PR events while
the PR bears the `macos-ci` label. It is a separate workflow so adding a
label does not restart or cancel the required Linux gate; it therefore runs beside Linux rather
than waiting for Linux success. It builds the same workspace without debug info/incremental
compilation and runs nextest with a 25-minute test-step timeout, followed by Clippy. The
Codex control tests are not filtered out. Namespace provides job-isolated runners rather than
reusing the fleet's long-lived target lanes and macOS debug-object cleanup policy.

Namespace runs these jobs through its GitHub App. If the app loses access to this repository, or
the profile has no capacity, jobs queue with no matching runner. A queued required check is not
a pass. Do not silently fall back to hosted or fleet runners.

`CI_RUN_ID` keeps the messaging-fault evidence under `target/messaging-faults/`, which is
uploaded with the stage logs.

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

## Merge queue

`main` lands through GitHub's merge queue. Add a ready pull request to it with

```sh
gh pr merge NUMBER --auto
```

The queue tests the pull request on top of the current `main` and the entries ahead of it with
`linux-gate`, `isolation-vm` and `genie-freshness` (these run on the `merge_group` event; see the
trigger in `fleet.yml.genie.ts`) and merges it with a merge commit when they pass. The pull
request does not need to be rebased onto the latest `main` first. A draft cannot be queued. If a
queued check fails, the entry leaves the queue and the pull request page says why: fix it and
queue it again. The merge train (`st lanes join smalltalk`) is retired.

The ruleset (`.github/repo-settings.json`, generated from `repo-settings.json.genie.ts`, applied
by an administrator and never by CI) requires the three checks from GitHub Actions with an empty
bypass list, keeps the pull-request, deletion and force-push protections, and configures the queue:
merge method MERGE, up to five entries build at once (see [Measured concurrency](#measured-concurrency)),
up to five merge together, and a check that
never reports fails its entry after 30 minutes. Repository settings enable native auto-merge and
branch deletion after merge. Check the live settings against the file with `gh-check-settings`:

```sh
nix run github:overengineeringstudio/effect-utils/3089f7e1faa82d7a4cb4de0e8d485164f837708b#gh-check-settings -- --repo compoundingtech/smalltalk --file .github/repo-settings.json
```

`st/ci` is no longer required. Its producer on the fleet's machines is retired after the first
pull request has merged through the queue.

To roll back, restore the previous ruleset (the JSON is in the body of pull request #916) with
`gh api --method PUT repos/compoundingtech/smalltalk/rulesets/20563764 --input old-main-ruleset.json`,
then start the train again with `st missions start` on its mission.

## Measured concurrency

The [manual capacity run](https://github.com/compoundingtech/smalltalk/actions/runs/36931429222)
on 2026-10-01 recorded the workspace limits with `nsc workspace concurrency --output json`:

| Platform | Concurrent vCPUs | Concurrent memory |
| --- | ---: | ---: |
| Linux (amd64 and arm64 share a pool) | 320 | 640 GiB |
| macOS arm64 | 96 | 224 GiB |

Namespace limits CPU and memory per platform; a workflow run is not a fixed unit of capacity.
Each Workspace CI group initially starts three 16-vCPU/32-GiB stage jobs and two
8-vCPU/16-GiB profile jobs: 64 vCPUs and 128 GiB at peak. Five complete groups fit the Linux
limit, which matches `max_entries_to_build: 5` in both the generated and live main rulesets.
PRs, main pushes and other workloads share that capacity; Namespace queues jobs until resources
are available. The `linux-gate` aggregate starts after the three stage jobs finish, so it does
not add to the initial peak. macOS uses its own pool.

At 21:51:47 UTC, GitHub's job step timestamps showed seven PR, merge-group and main workflow
runs executing 19 Namespace jobs together. Including the manual capacity run, the overlap was
eight runs and 24 jobs. These are observed overlaps of runs at different stages, rather than
eight fully parallel Workspace CI groups. The runs were:

- main: [Workspace CI](https://github.com/compoundingtech/smalltalk/actions/runs/36930646852)
  and [macOS CI](https://github.com/compoundingtech/smalltalk/actions/runs/36930646948) for one commit,
  with [Workspace CI](https://github.com/compoundingtech/smalltalk/actions/runs/36931186016)
  and [macOS CI](https://github.com/compoundingtech/smalltalk/actions/runs/36931186029) for the next;
- merge group: [Workspace CI](https://github.com/compoundingtech/smalltalk/actions/runs/36930644645);
- PRs: [Workspace CI](https://github.com/compoundingtech/smalltalk/actions/runs/36931199367)
  and [Workspace CI](https://github.com/compoundingtech/smalltalk/actions/runs/36931346163).

The initial jobs in 13 observed CI runs created from 21:30 UTC had a median startup delay of
18 seconds, a 95th percentile of 187 seconds (nearest rank) and a maximum of 305 seconds (57 jobs). Startup
delay is measured from workflow creation to the first job step; jobs waiting on dependencies,
skipped jobs and jobs without a Namespace runner are excluded. Each active job's interval runs
from its first step to job completion, with unfinished jobs counted through the observation.
The observation is repository-scoped; the workspace can also have jobs from other repositories.

To refresh the capacity measurement, dispatch Workspace CI on `main`. Its `namespace-capacity`
job runs only for `workflow_dispatch`, publishes platform limits and current usage to the job
summary and retains the `namespace-capacity` artifact. It reports no workspace or account identity.
See Namespace's [resource limits](https://namespace.so/docs/architecture/compute/resource-limits)
and [profile concurrency controls](https://namespace.so/docs/solutions/github-actions/runner-controls/concurrent-runners)
for the scheduler's limits, and GitHub's [merge queue settings](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)
for the distinction between build concurrency and merge batch size.

## Namespace jobs that never start

Jobs normally start 8 to 25 seconds after they are created. Once in more than 150 jobs a Namespace
job stayed `queued` with no runner (13 minutes and counting). A plain `gh run cancel` does nothing
to it. Recover with the force-cancel endpoint and a rerun:

```sh
gh api --method POST repos/compoundingtech/smalltalk/actions/runs/RUN_ID/force-cancel
gh run rerun RUN_ID
```

A queued Namespace job is never a pass. Do not fall back to another runner.

## Inspect a failure

Use the PR Checks tab or `gh run view RUN_ID --log-failed`. The Linux job uploads `linux-ci-logs`
even on failure. Inspect each stage's log and timing, the selected suite and checked merge SHA.
A passing retry is a flaky outcome in the nextest log. A queued Namespace job with no runner
is infrastructure readiness, not a successful check; the merge queue keeps the entry waiting.

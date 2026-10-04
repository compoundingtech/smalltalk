# CI operations

## GitHub Actions on Namespace

Every push to `main` runs both `Workspace CI` and `macOS CI` on Namespace. Each non-PR run
uses its own `github.run_id` in the concurrency group, so successive pushes can run concurrently
without cancelling running checks or replacing pending runs. PR updates still cancel stale
checks for that PR; macOS checks on PRs require the `macos-ci` label.

The generated `Workspace CI` workflow (`.github/workflows/fleet.yml`) and `macOS CI`
(`.github/workflows/macos.yml`) replace the fleet's former Linux `st/ci` and optional `st/ci-macos`
execution. The required checks on `main` are `linux-gate`, `isolation-vm`, `genie-freshness` and
`typescript-client` (apply the fourth check only after its first successful main run),
and `main` lands through GitHub's merge queue (see [Merge queue](#merge-queue)).

Every pull request, including a fork and a draft, gets the Linux gate, the isolation VM and the
freshness check. `Workspace CI` also runs on the `merge_group` event, so GitHub's merge queue receives
the required checks for each queued entry.
Checkout uses GitHub's default `pull_request` merge ref, not the contributor's unmerged
head: it tests that head merged with the current base. Strict branch protection also requires
that the head itself contain the latest `main`. No `pull_request_target` job runs PR code,
and the gate has only `contents: read` permission. Forks do not receive publishing secrets.

The Linux gate runs as three jobs on separate runners, so they no longer share one machine's CPUs.
`linux-gate` is the single required check: it needs the three jobs and passes only when every one of
them succeeded (a skipped or cancelled stage fails it). The stage jobs use the shape label
`nscloud-ubuntu-24.04-amd64-8x16`; `genie-freshness`, `isolation-vm`, `typescript-client` and the `linux-gate`
aggregate use `namespace-profile-linux-x86-64`. The stages ran on `nscloud-ubuntu-24.04-amd64-16x32`
until 2026-10-03, when that label stopped getting runners; on the profile they queued behind its
limit of about five runners at once. `scripts/ci-linux STAGE` runs one stage:

- `linux-tests`: prepares the provider component fixtures, installs matching rendered st2 hooks,
  builds the selected test executables with dev/test debug info and incremental compilation
  disabled, then runs `bash scripts/ci-nextest run` on every CPU, selected
  by the profile's default filter (see [gate scope](#gate-scope));
- `linux-clippy`: `cargo clippy --workspace --all-targets --locked`, then
  `cargo run --locked -p st3-client-codegen -- --check`;
- `linux-fleet-compat`: the fleet compatibility test against `.github/fleet-compat-baseline.json`'s
  pinned older st3. Building that baseline also runs the pinned pty's own unit tests, two of which
  are timing-sensitive, so the build is retried up to three times.

Each stage restores a job-keyed `actions/cache` entry (Namespace serves it from its accelerated
backend) holding Cargo's registry and the workspace `target/` directory, keyed on `Cargo.lock` and
`flake.lock`. A second keyed entry (`nix4-<job>-...`) holds a signed local Nix binary cache in
`$RUNNER_TEMP/st-ci-cache`. Its key includes `flake.lock` and both compatibility baseline pins.
`scripts/ci-nix-cache use` makes it a preferred substituter. After a successful stage, `save`
copies reference-free downloads and sources fetched by the run, plus the closures of the fleet
baseline, historical messaging channel and provider components. It leaves the installer-managed
`/nix` directory intact. Cache failures emit a warning and let the job build normally.

The [original trial measurements](https://github.com/compoundingtech/smalltalk/pull/849#issuecomment-5936374396)
recorded a warm run of 6m59s with this design, versus 8m49s to 9m30s with only the Cargo cache.
That trial excluded the messaging matrix; current CI includes it. Caching the entire Nix store
instead took 90–105 seconds to restore 4.4 GB, which cost more than it saved. The selected
outputs keep the cache focused on repeated downloads and expensive immutable builds.
Namespace cache volumes are not used: they are per node and replicate in the background, so a
job landing on another node can start empty.

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

### Boot canaries

`boot_canaries::*` in `linux-tests` boots real seats of every harness st drives (Claude, Codex, pi,
omp, OpenCode) against a real st3 daemon and real `pty` sessions, with a token-free stand-in for
the provider (`scripts/st3-boot-canaries`; see its README). Five scenarios per harness: a fresh
seat, a restarted seat, a daemon restart while the predecessor is still the latest runtime
observation, a driver re-exec into a replaced binary, and five seats launching at once. Each seat
must claim its step with its current incarnation and read an st message within the time bound, and
must not park in the crash-loop guard. The stand-ins answer instantly, where real providers take
seconds, so a race between a provider and the daemon fails here every time instead of on whichever
launch loses it. These cases never retry: a pass on the second attempt is the race the canary
exists to catch (`.config/nextest.toml`). A failed case keeps its daemon log, the seat's terminal,
its claim trace and the stand-in's receipts under `target/boot-canaries/`, which the stage uploads.

The Codex stand-in's schema files are generated from the protocol gate's own fixture; when the
required Codex methods change, `ST_REGENERATE_CODEX_STUB_SCHEMAS=1 cargo test -p st-drivers
the_boot_canary_codex_stub_schemas` rewrites them.

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
workspace. List the selection with `bash scripts/ci-nextest list`.

`scripts/ci-nextest` supplies Cargo's target selection for both the build-only step
(`bash scripts/ci-nextest run --no-run`) and execution, on Linux and macOS. It runs two groups:
the workspace excluding st2, then st2's `integration`, `agent_resource` and `driver_expansion`
test targets. Both use the unchanged `ci` profile, and both run even if the first fails; any
failure fails the stage. This avoids compiling st2's entirely excluded lib/bin unit-test
targets, `event_e2e` and `resource_profile_supervisor_e2e`. Nextest filters alone do not avoid
compiling them. Partially selected binaries (st2 `integration` and st-drivers' library tests)
still compile in full, and the retained st2 tests still build the st2 executable they need.
When adding a st2 test target or changing binary-level exclusions, update the script's
retained target list alongside `.config/nextest.toml`.

The old trusted fleet runner and st merge train are retired; there is no operations installation
handoff. Namespace workflows use this script from the checkout and land through GitHub's merge
queue. Provider fixtures, rendered hooks, workspace Clippy and the isolation archive keep their
existing scope.

### Current-product boundary

Every required Linux stage runs `scripts/check-st-boundary-test` and
`scripts/check-st-boundary`. The source guard rejects `st2::` imports and `extern crate st2`
in st3/stui, plus direct, renamed or indirect workspace dependencies on st2 (including
optional and target-specific dependencies). st2 remains independently buildable/testable.

The Claude no-st2 seat eval and all five harness boot canaries inspect fresh seat processes
for `ST2_*` exports and st2 program/path references. They also inspect generated state, home
and PTY paths, text records, SQLite schemas/semantic rows and logs for st2 labels. Fixtures use
neutral identities and payloads, so authored text cannot hide an owned label. Base64 replication
payloads/signatures are opaque; their stored semantic claims are checked separately. Historical
binary-upgrade canaries retain predecessor records and are outside this fresh-generation rule.
The mutation suite injects every prohibited category and requires rejection; source/dependency
mutations also exercise the guard's CLI exit status.

Fresh trust writes use `.st-trust.lock` and `.st-trust.<pid>` staging files. If a historical
trust lock already exists, the new writer also holds it without replacing or creating it;
both generations continue to coordinate with Claude's own config lock.

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

### TypeScript client

`typescript-client` runs on every PR, merge-group entry and main push. It installs Node 24.18.0
(the workspace uses Node 24), TypeScript 6.0.3 from the iOS lockfile and the client's pinned
`effect@4.0.0-rc.118` development dependency. Both `node_modules` directories are cached together,
keyed by both lockfiles and the Node version; a miss runs `npm ci --ignore-scripts` in each package.
The lockfile fingerprint uses `sha256sum` in Bash so ci1's Nix runner needs no Node 20
`hashFiles` helper when it evaluates the cache key.

`bash scripts/ci-typescript-client` runs the client's contract and schema tests with `node --test`,
the README's strict raw-client and rich-schema typechecks, and `tsc --noEmit -p apps/ios`.
The iOS project allows explicit TypeScript import extensions for generated-client consumers.
The job starts non-required. The generated repository settings stage its promotion; an administrator
must apply that fourth check to the live main ruleset only after a successful main run.

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
a pass. Apart from the ci1 choice below, which is made once per run before any job starts, do not
fall back to hosted or fleet runners.

### ci1: our own runners, with Namespace as overflow

ci1 is a dedicated machine of ours that runs GitHub self-hosted runners for this repository, with
warm caches kept on the machine. GitHub has no overflow between runner labels, so `Workspace CI`
starts with `pick-runner`, a GitHub-hosted job that lists the organization's self-hosted runners
through the API and picks one pool for the whole run:

- `ci1` when at least `CI1_MIN_IDLE` (default 5, the jobs a run starts at once) runners with that
  label are online and idle; merge-group runs ask for `ci1-merge`, which a runner reserved for the
  merge queue also carries, so queued merges never wait behind pull request pushes;
- Namespace otherwise, exactly as above: when ci1 is busy or offline, when the runner list is
  unavailable, and always for a pull request from a fork. The repository is public and a self-hosted
  runner runs whatever a job asks, so fork code never reaches ci1 (and forks receive no secrets).

Every other job's `runs-on` reads `pick-runner`'s output and falls back to its Namespace label when
the output is empty. The job names and the `linux-gate` aggregate are unchanged; `linux-gate` now
names its three stages instead of `needs.*`, because `pick-runner` is skipped whenever ci1 is off.
Two runs that pick at the same moment can both choose ci1; the later run's jobs then wait for
runners on ci1.

The switch is the repository variable `CI1_RUNNERS`: unset (the default), `pick-runner` is skipped
and every run goes to Namespace with no extra job. `on` turns the choice on, and unsetting it turns
it off again without a pull request. `pick-runner` reads the runners with the
`CI1_RUNNERS_READ_TOKEN` secret, a token that may only read the organization's self-hosted runners;
without it every run goes to Namespace.

On ci1 each runner is ephemeral: it takes one job, runs it as its own user in a fresh work directory
with its own `/tmp`, and nothing the job started outlives it. The runner names a Cargo home in
`CI_LOCAL_CARGO_HOME`; the stage jobs then skip the `actions/cache` restores and the local Nix
cache, use that Cargo home, and Cargo keeps its intermediate build files in a per-runner build
directory, while sccache shares compiled crates between all runners and the Nix store is the
machine's own. The machine's configuration lives in the private network repository.
Initial Cargo build and nextest concurrency on ci1 is four threads per runner, with a 14 GiB
per-job memory limit; tune those limits from measured runs on the machine.

`CI_RUN_ID` keeps the messaging-fault evidence under `target/messaging-faults/`, which is
uploaded with the stage logs.

### Performance gate

Two jobs check the daemon's rules that reads are instant, writes are short, and no query's cost
grows with the whole store. Neither is part of `linux-gate`; making one required is a decision
for the repository's owner. Both run `scripts/ci-perf`, and `.config/nextest.toml` keeps their
tests out of `linux-tests`.

`perf-cost` runs `daemon_cost::` (`crates/st3/tests/daemon_cost.rs`) on every pull request and
`main` push, on the stages' runner. It skips
merge-queue entries, which wait only for required checks, so a queued entry needs no more runners
than before (see [Measured concurrency](#measured-concurrency)); if it becomes required, it must
run there too. It generates a store at scale 0.01 and one at 0.1 with the
`daemon_bench` generator, serves each from an in-process daemon, and counts the SQLite work of
every route: virtual machine steps, steps through a table without an index, sorts and
auto-index rows, read from each statement's counters as it finishes
(`smallclaims::sqlite::work`, built only with `test-support`). A request fails when its work at
the larger scale is more than three times its work at the smaller, after dividing by how much its
answer grew. It also measures a replication round as the worker runs it (summary, push, receive)
and a checkpoint trim, per deleted row. Counts do not depend on the machine, so the job builds
with `opt-level = 1` only to generate the stores faster.

- Every route `api.rs` declares is measured or listed in `NOT_MEASURED` with its reason; a new
  route fails `the_cost_check_covers_every_route` until it is one or the other.
- `KNOWN_GROWTH` lists the routes whose work already grew with the store when the check
  landed, each with a ceiling of half again its measured growth. A listed route fails if it grows
  past its ceiling, and fails once fixed until it leaves the list.
- The check failed on both regressions that reached production: the shape of #814 (a correlated
  canonical-order subquery in the document reads; `GET /v1/documents` went from 84,865 to
  8,007,288 steps for a store ten times larger) and the foreign-key columns #1103 indexed (a trim's
  work per deleted row grew 7.3 times, its full-scan steps 9.3 times).

`perf-load` runs `daemon_load::` in a release build, in its own `Performance` workflow
(`perf.yml`): nightly on `main`, on pull requests that change the daemon or the store, and on
dispatch. It serves a store the size of a busy host's (scale 1, about 240,000 claims) to the
request mix and rates that host's daemon reported in its busiest five-minute window (30 requests a
second: harness events, mailbox pages, claims, desired state, delivery holds, replication rounds,
renewals, status and work reads, and a person's reads), with the reconciler running and 30
concurrent seat event long-polls. Quiet polls have a 31-second budget for their intentional
30-second wait; mailbox WebSockets require authenticated native drivers and are excluded.
It fails when
a request's p99 or the daemon's CPU passes its budget, or is more than 20% worse than the worst of
main's last five reports: one run's p99 on a shared runner can be twice the next run's, so a
regression is what passes several. Main's successful runs add their report
(`perf-load-baseline-*` in the Actions cache). Relative latency comparisons start once five
main reports exist; until then every path still checks its absolute p99 budget and every request
error fails. CPU compares as soon as one main report exists, because it averages the whole run.
Relative latency tolerates 5 ms of noise, or 50 ms when either path has fewer than 50 samples:
those sparse p99s are effectively observed maxima. This bounded tolerance still catches large
regressions on rare paths. CPU tolerates 0.05 cores. A run without any baseline checks only the
absolute budgets and errors. It is
not in Workspace CI because with warm caches it takes as long as `linux-tests`, and twice as long
when its stores must generate.

Both jobs keep their generated stores in the Actions cache, keyed by the generator and
`docs/st3/schema.md`, so a store from an older generator is never measured.

Run either locally with `TMPDIR=/var/tmp`; `ST_BENCH_DIR` keeps the generated stores between runs:

```sh
cargo test -p st3 --test integration daemon_cost:: -- --nocapture --test-threads 1
ST_LOAD_GATE=1 cargo test --release -p st3 --test integration daemon_load:: -- --nocapture
```

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
| `release-smalltalk.yml` | Preserve tag/dispatch/fork-PR triggers, target packaging, source verification and publishing permissions. Move Linux and ARM macOS runners to Namespace. Also run on every `main` push, keeping the archives as 7-day artifacts, so release breakage fails on `main` (not required for merging). |
| `release-daily.yml` | New. Once a day, publish the archives of the newest successful `main` release run when `main` changed since the last release; see [binary releases](st3/binary-releases.md#daily-releases). |
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
`linux-gate`, `isolation-vm`, `genie-freshness` and `typescript-client` (these run on the `merge_group` event; see the
trigger in `fleet.yml.genie.ts`) and merges it with a merge commit when they pass. The pull
request does not need to be rebased onto the latest `main` first. A draft cannot be queued. If a
queued check fails, the entry leaves the queue and the pull request page says why: fix it and
queue it again. The merge train (`st lanes join smalltalk`) is retired.

The ruleset (`.github/repo-settings.json`, generated from `repo-settings.json.genie.ts`, applied
by an administrator and never by CI) requires the four checks from GitHub Actions with an empty
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
With the current 8x16 stage runners, a merge-queue Workspace CI group initially starts three
8-vCPU/16-GiB stage jobs and two 8-vCPU/16-GiB profile jobs: 40 vCPUs and 80 GiB at peak.
The TypeScript client job follows generator freshness and reuses its runner slot.
PR and main runs also start `perf-cost`, taking their initial peak to 48 vCPUs and 96 GiB.
Five complete merge-queue groups need 200 vCPUs and 400 GiB, within the Linux pool limit;
`max_entries_to_build` remains 5 in both the generated and live main rulesets.
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

A queued Namespace job is never a pass. Do not fall back to another runner by hand; only
`pick-runner` chooses between ci1 and Namespace, before a run's jobs start.

## Inspect a failure

Use the PR Checks tab or `gh run view RUN_ID --log-failed`. The Linux job uploads `linux-ci-logs`
even on failure. Inspect each stage's log and timing, the selected suite and checked merge SHA.
A passing retry is a flaky outcome in the nextest log. A queued Namespace job with no runner
is infrastructure readiness, not a successful check; the merge queue keeps the entry waiting.

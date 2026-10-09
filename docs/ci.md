# CI operations

## GitHub Actions on Namespace

Workspace CI runs on pull requests, merge groups and manual dispatch. The merge queue tests the
exact commit that lands on `main`; its successful checks stay attached to that SHA, so a main push
does not repeat the workspace gate. `Main upkeep` verifies those five checks, runs `perf-cost`
(which the queue skips), and fills missing main-scope caches on Namespace. macOS CI is currently
disabled. PR updates cancel obsolete runs for that PR in Workspace CI, Performance and the
public-content guard. Main upkeep, Performance and public checks keep only the latest main push;
older pushes must not keep consuming runners after their source is superseded. Manual dispatch,
scheduled Performance controls and merge-group runs retain independent `github.run_id` groups.
The required queue checks and their runner reservations are unchanged.

The generated `Workspace CI` workflow (`.github/workflows/fleet.yml`) replaces the fleet's
former Linux `st/ci` execution. The retained macOS workflow (`.github/workflows/macos.yml`) is
disabled in GitHub Actions; it does not currently run on PRs or main pushes. The required checks
on `main` are `linux-gate`, `isolation-vm`, `genie-freshness`,
`typescript-client` and `mail-redelivery-canaries`,
and `main` lands through GitHub's merge queue (see [Merge queue](#merge-queue)).

Every pull request, including a fork and a draft, gets the Linux gate, the isolation VM, the
freshness check, the TypeScript client check and the mail redelivery canaries. `Workspace CI` also runs on the `merge_group` event, so GitHub's merge queue receives
the required checks for each queued entry.
Checkout uses GitHub's default `pull_request` merge ref, not the contributor's unmerged
head: it tests that head merged with the current base. The merge queue combines the head with
the current `main`; the head does not need to be rebased first. No `pull_request_target` job runs PR code,
and the gate has only `contents: read` permission. Forks do not receive publishing secrets.

The fast `upgrade-impact` job starts independently on GitHub-hosted `ubuntu-latest`, using only
Python and Git. The adoption boundary is **PR #1661**: PR numbers above it require a fresh valid
upgrade fragment, including documentation changes. PRs #1661 and below without a fragment retain
their prior merge policy; they still require classification before public release. Supplied
fragments are validated, including schema/rules transitions, and existing fragments are immutable.
PR events supply their number; queue refs identify a single PR, while multi-PR queue groups
identify each member by its merge subject so an old head PR cannot exempt a newer member. It checks
the effective PR merge against that merge commit's first parent, and each integrated PR in a
merge group against the group's supplied base. A PR event's recorded `base.sha` can be stale
after main advances; it is not the PR check's comparison base. Both bases stay pinned to the
tested source rather than a later `origin/main` fetch;
unrelated unreleased main changes do not block a new PR's check. It does not download dependencies
or compile. Manual dispatch runs the safety tests without inventing a PR delta.

The Linux gate runs two test partitions plus Clippy and fleet compatibility on separate runners.
`linux-gate` is the aggregate Linux check: it needs `upgrade-impact`, the four stage jobs and the named mail redelivery
check, and passes only when every one succeeded (a skipped or cancelled stage fails it). The stage jobs use the shape label
`nscloud-ubuntu-24.04-amd64-8x16-with-features`; `genie-freshness`, `isolation-vm` and `typescript-client`
use `namespace-profile-linux-x86-64` when they overflow. The `linux-gate` aggregate uses GitHub-hosted
`ubuntu-latest`, so it cannot queue behind build or benchmark jobs. The stages ran on `nscloud-ubuntu-24.04-amd64-16x32`
until 2026-10-03, when that label stopped getting runners; on the profile they queued behind its
limit of about five runners at once.

Required Workspace checks for PRs and merge groups use Namespace `job.priority=1`.
Optional Performance and `perf-cost`, manual controls, main upkeep and releases retain
priority 2. Required PR checks must finish before a PR can enter the merge queue; merge-first
ordering previously allowed new merge jobs to overtake these prerequisites. This policy orders
waiting jobs and neither preempts running work nor reserves capacity. The picker, local pools,
runner shapes, run affinity and required checks are unchanged.

The 2026-10-09 investigation retained a PR build waiting 55.1 minutes at priority 2 before
15.7 minutes of execution; two sampled merge builds at priority 1 waited 0.3 and 1.7 minutes.
One 116.6-minute PR had 41.0 minutes of producer wait and 28.8 minutes of shard wait.
This motivates queue ordering; it does not establish a cache or compiler defect, nor promise
that the 20-minute run SLO is met under overload.

Profile labels carry affinity inline, for example
`namespace-profile-linux-x86-64;job.priority=1;github.run-id=${{ github.run_id }}`.
Shape labels retain `-with-features` and a separate
`namespace-features:github.run-id=${{ github.run_id }}` label. Namespace does not support a
separate `namespace-features:` label with profiles; see the
[Runner Controls syntax](https://namespace.so/docs/solutions/github-actions/runner-controls).

The documented Linux limit is 320 vCPUs / 640 GiB; this change does not increase it.
Queue time is measured separately from execution time. Already queued jobs retain the labels
from their immutable workflow revision and finish naturally; this change does not rerun them.
Optional jobs remain subject to capacity after required checks. See Namespace's
[job ordering and priority controls](https://namespace.so/docs/solutions/github-actions/runner-controls/job-ordering).

`scripts/ci-linux STAGE` runs one stage:

- `linux-tests` and `linux-tests-shard-2`: prepare the provider component fixtures, build the
  selected test executables with dev/test debug info and incremental compilation disabled,
  and install matching hooks directly from the built st2 executable. They run complementary
  nextest hash partitions with eight test threads each. The profile's default filter still
  selects the gate scope (see [gate scope](#gate-scope));
- `linux-clippy`: `cargo clippy --workspace --all-targets --locked`, the standalone conversation
  model check, the [warning ratchet](#clippy-warning-ratchet), then
  `cargo run --locked -p st3-client-codegen -- --check`;
- `linux-fleet-compat`: the fleet compatibility test against `.github/fleet-compat-baseline.json`'s
  pinned older st3. Building that baseline also runs the pinned pty's own unit tests, two of which
  are timing-sensitive, so the build is retried up to three times.

The primary test job proves that the two actual nextest inventories are disjoint and their union
equals the full selected suite. The explicit zero-retry mail canaries run in a parallel job. The primary also
runs the standalone conversation model feature check. The second shard runs the remaining
workspace and st2 tests on independent Namespace CPUs. `linux-gate` requires both shards;
failure, cancellation or skipping either shard fails the gate. Each shard retains passing test
durations in its logs, and the primary saves `ci-logs/test-partitions.json` with the tested SHA
and all three inventories.

The Linux `linux-test-build` job compiles the existing two selected Cargo groups once and
publishes nextest archives. Both shards, the zero-retry mail job and isolation VM require that
successful producer and download its exact artifact ID. They check the manifest SHA, source/tree,
run, the successful producer attempt, architecture, nextest/rustc versions and each archive digest before extracting.
A failed-jobs-only rerun can inherit the earlier successful producer output; its attempt and
manifest remain explicitly pinned, with the same run/source/tree/tool identities required. Cargo
target caches remain on the producer; consumers restore only runtime/Nix fixtures. The producer also runs the unchanged standalone conversation-model tests (including doctests)
and dependency boundary, and builds the gateway binary with its standalone production features.
Its Cargo JSON and binary hash are retained alongside the archives; the VM does not substitute
the workspace-unified test binary. A failed producer explicitly fails consumer checks and the gate.

Tests resolve archived executables through nextest's runtime binary paths. A source guard rejects raw compiled binary, manifest and temporary-directory lookups in test code.
Manifest/fixture paths
map each compiled package beneath the recorded producer root to the consumer checkout, including
library fixtures launched by another package. Outside archives the original compiled path remains
the fallback. Archive mode rejects missing or mismatched roots/binaries. Tests, assertions,
partitions, retries, eight test threads, real VM checks and required contexts are unchanged.
Local and macOS runs still use the existing build selection unless `CI_TEST_PARTITION` is set.

New test files and paths are checked automatically by `genie-freshness` on each PR and
merge group. A guard failure names each file and line and points its author to `test_env!`
and these repair instructions; it does not allow the raw path through.
If `genie-freshness` reports `unrelocated test paths: FILE:LINE`, replace the raw test path
at that location with `test_env!("CARGO_BIN_EXE_st3-fixture")`,
`test_env!("CARGO_MANIFEST_DIR")` or `test_env!("CARGO_TARGET_TMPDIR")`, as appropriate.
Use `test_env!("CARGO_MANIFEST_DIR", "/relative/fixture")` instead of a `concat!` fixture
path, and `test_bin!("st3-fixture")` instead of `cargo_bin!`. The integration test root and
test-enabled libraries already import `scripts/ci-test-paths.rs`; a separate test target
must import that helper with `#[macro_use]` and a `#[path = "..."]` relative to its source
file. Keep the original fixture, executable, arguments and assertions. Run
`python3 scripts/check-ci-test-paths` and the affected test before pushing. Apply the same
fix if a merge group finds a raw path introduced by another PR; do not bypass the guard.

Main upkeep probes the exact Cargo and Nix cache keys for each stage before provisioning Nix
or restoring build archives. When both entries exist it stops after the probes. A miss is flagged
as P0 and fills the missing entries with builds only: selected test executables, Clippy artifacts,
the fleet baseline and integration binary, the Genie shell, or the isolation archive and VM driver. Workspace tests and the isolation VM are not repeated on main. The TypeScript dependency cache is also kept on main without repeating its tests.
Merge-group and PR caches have their own ref scope; they cannot replace these main-scope saves,
which all PRs and Namespace overflow runs can restore. Manual Workspace CI dispatch on main
remains available for a full run. Release and deployment workflows keep their own push triggers.

Each stage restores a job-keyed `actions/cache` entry (Namespace serves it from its accelerated
backend) holding Cargo's registry and the workspace `target/` directory, keyed on `Cargo.lock` and
`flake.lock`, workspace manifests and linker configuration. A second keyed entry (`nix5-<job>-...`) holds a signed local Nix binary cache in
`$RUNNER_TEMP/st-ci-cache`. Its key includes `flake.lock`, the flake, Nix expressions and both compatibility baseline pins.
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
Recovery cases include `recovery_timing` in the result printed by a failed test: partition
restoration, the observed current channel and its incarnation, the first peer exchange request,
staging acceptance, native consumption, and read acceptance. Staging precedes writing the
native frame and does not prove an accepted offer. These timestamps and offsets use
the recovery restoration time, independently of the later fresh send. `recovered-trace.json`
retains the recovered message's graph claims. A peer request proves transport activity, not
the recipient's local message arrival; channel readiness records an observation, not its first transition.

`.config/nextest.toml` gives the messaging fault matrix and the fleet reconnect test, both with
real multi-minute outages, first priority so their retries fit the CI test window. Failed tests
retry twice with fixed 30-second delays; a retry pass is reported as flaky, not a gate failure.
Each run isolates test `HOME` and XDG state. The summary records the tested SHA,
each stage's elapsed time, result and exit code; each stage job uploads `<job>-logs` with its log and `.time` file.
Nextest's final summary retains flaky outcomes.

st3 integration fixtures use the separate `st3-fixture` executable, built automatically by
the test-only `test-support` dev dependency. It captures an isolated Bash environment,
reads only the fixture HOME's `.bash_profile`, preserves the fixture PATH, and ignores the
launching seat's process ancestry. The production `st3` target has no runtime flag or
environment variable that enables this behavior, even if the executable is renamed.
Fixture command helpers clear inherited `ST_AGENT`/`ST3_*`; temporary-repository Git helpers
isolate global/system config, hooks, signing and author identity on each command. Real
repository commits keep the host's Git policy. Run the same suite from an agent seat with
`nix develop --command cargo nextest run -p st3 --locked --profile ci --retries 0`.
Provider fixtures that load st3 extensions publish their isolated hook set with
`st3::hooks::ensure_installed`, preserving every declared asset and relative import.

Boot, delivery-probe and messaging-fault fixtures put large executable copies in Cargo's target scratch
directory, keeping their Unix sockets in short temporary paths. This avoids exhausting a
host's temporary-filesystem quota when debug binaries are copied by parallel cases.

On Linux, tests that launch isolated daemons, drivers, PTYs or long-lived CLI children call
`st3::test_support::supervise_test()` before creating their fixtures. It runs that exact test
under `scripts/st3_test_process.py`, a detached subreaper watching pidfds for the Rust runner
and its launcher. Success, failure, panic, timeout, SIGTERM and SIGKILL of the runner all
end the owned process tree. Each test has its own process group; detached children are
adopted and killed and reaped before the supervisor returns the test's status. Exited adopted
PTYs are also reaped while the test runs, so stop/suspend checks see their PIDs disappear. Cleanup
uses only the supervisor's descendants, including when evidence collection fails or a
temporary binary directory has already been removed. The Python boot, no-st2, subagent,
messaging-fault and delivery-probe entrypoints use the same supervisor when run directly.

The isolated-process audit covers `boot_canaries`, `broken_gates`, `daemon_environment`,
`first_sync`, `fleet`, `getting_started`, `mission_cancellation`, `daemon_restart`,
`idle_budget`, `driver_incarnation`, `codex_bootstrap`, `no_st2_seat`, `subagents_seat`,
`delivery_probe`, and `messaging_faults`. The process-spawning tests in `action_coverage`,
`agents_restart`, `hook_telemetry`, `terminal_attach` and `command_recorder` use it too.
The separate `api_accept` target also supervises its descriptor-exhaustion subprocess.
The remaining daemon fixtures serve their APIs in process or use fake runtime observations;
their tasks end with their test runtime. New process-spawning fixtures should use this helper.

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

`boot_canaries::every_exit_reaps_the_daemon_even_when_the_rust_test_is_killed` starts the real
boot-canary Node in disposable Rust test processes. It covers success, assertion failure,
Rust panic, SIGTERM and SIGKILL of the Rust process, and SIGKILL of the Python fixture. It
requires both the daemon and a double-forked child in a new session to disappear within
five seconds, and separately injects an evidence-collection error to prove `node.stop()`
still runs. Existing canary assertions, phase bounds and zero retries remain unchanged.

The Codex stand-in's schema files are generated from the protocol gate's own fixture; when the
required Codex methods change, `ST_REGENERATE_CODEX_STUB_SCHEMAS=1 cargo test -p st-drivers
the_boot_canary_codex_stub_schemas` rewrites them.

### Clippy warning ratchet

`linux-clippy` reuses JSON diagnostics from its existing workspace and standalone
`st3-conversation-ui` Clippy runs. `.github/clippy-baseline.json` records warning counts per
workspace crate and lint, with sorted keys and the compiler/Clippy versions supplied by the
`flake.lock`-pinned devShell. It includes Rust warnings as well as Clippy warnings, ignores
dependencies outside the workspace, and counts each source diagnostic once when Cargo repeats
it for library/test targets or the standalone model run. A cached Cargo run replays diagnostics.
Errors and deny-level lints still fail Cargo; a failed, truncated or malformed diagnostic stream
cannot pass the ratchet.

Any crate/lint count above its baseline fails, including a newly introduced crate or lint.
Removing warnings also requires lowering the baseline in the same pull request, so a later
change cannot spend the removed warnings. CI checks that the committed counts do not increase
relative to the event's immutable base SHA (PR base, merge-group base, or preceding main commit).
The first rollout permits a base without the file. There is no new required check:
`linux-gate` already requires `linux-clippy`.

To check locally and lower the baseline after fixing warnings:

```sh
export RUNNER_TEMP=$(mktemp -d)
export GITHUB_STEP_SUMMARY="$RUNNER_TEMP/summary.md"
nix develop -c bash scripts/ci-linux clippy
# If the stage reports removed warnings, use its saved diagnostics without rebuilding:
python3 scripts/clippy_ratchet.py --logs "$RUNNER_TEMP/ci-logs" --update
git add .github/clippy-baseline.json
nix develop -c bash scripts/ci-linux clippy
```

`--update` refuses to increase any existing count. The job retains the diagnostic streams,
metadata and toolchain versions in its `linux-clippy-logs` artifact; the failure message gives
the same update command. When updating `flake.lock`'s Rust toolchain, regenerate the version
metadata using fresh diagnostics in the new pinned shell and fix any new warnings; counts
still cannot increase. Run the ratchet's focused regression proofs with
`python3 scripts/clippy-ratchet-test` (CI runs them in the same stage).

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
payloads and replication/claim signatures are opaque; stored semantic claims remain checked.
Historical binary-upgrade canaries retain predecessor records and are outside this fresh-generation rule.
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

`typescript-client` runs on every PR and merge-group entry. Main retains the queue check;
Main upkeep preserves the dependency cache. It installs Node 24.18.0
(the workspace uses Node 24), the client's pinned TypeScript 6.0.3 and
`effect@4.0.0-rc.118` development dependencies from its own lockfile. Both `node_modules`
directories are cached together, keyed by both lockfiles and the Node version; a miss runs `npm ci --ignore-scripts` in each package.
The lockfile fingerprint uses `sha256sum` in Bash so ci1's Nix runner needs no Node 20
`hashFiles` helper when it evaluates the cache key.

The client package's `npm test` runs the contract and schema tests with `node --test`;
`npm run typecheck` runs its strict compiler checks. CI installs and checks the client before
installing the iOS dependencies for the separate project typecheck.
`bash scripts/ci-typescript-client` runs the client commands and `tsc --noEmit -p apps/ios` locally
when both packages' dependencies are installed.
The iOS project allows explicit TypeScript import extensions for generated-client consumers.
The main ruleset requires `typescript-client`. It was enabled after the new job passed on main
in [#1256](https://github.com/compoundingtech/smalltalk/pull/1256).

### macOS

The macOS workflow is currently disabled in GitHub Actions. Its retained definition is described
below for reference; adding a label does not enable it.

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
through the API and picks the primary test partition's pool:

- `ci1` when at least `CI1_MIN_IDLE` (default 1) general runners are online and idle. Priority
  and merge-only workers are excluded from this ordinary pool;
- `ci1-priority` for trusted PRs labelled `ci-priority`, without an idle-count or token dependency.
  Pending urgent checks get the next free general runners; one slot stays reserved for priority
  and merge work after urgent checks finish;
- `ci1-priority` for a merge-group entry whose PR is labelled `ci-priority`, including after its
  PR checks passed. A read-only PR-label lookup identifies the entry from its queue ref;
- `ci1-merge` for other merge-group runs while ci1 is enabled. These jobs wait for their reserved
  pool even when its runners are currently busy, and do not query the status API;
- Namespace otherwise, exactly as above: when ci1 is busy or offline, when the runner list is
  unavailable, and always for a pull request from a fork. The repository is public and a self-hosted
  runner runs whatever a job asks, so fork code never reaches ci1 (and forks receive no secrets).

`linux-tests` reads the primary output and falls back to Namespace when it is empty.
The second shard and mail canaries read separate outputs, each choosing only `ci1` general
capacity. The picker counts online, idle general runners and subtracts one slot when the primary
may also use that pool. It offers the remaining slots to the second shard, then the canaries;
each falls back to the same Namespace priority and run affinity when no slot remains. The
primary's admission threshold does not strand idle slots for these two extra jobs. A previous
threshold of four sent the primary to Namespace even when three general workers were free;
the one-slot default lets both shards and canaries use those three workers.
Neither extra output can select `ci1-priority` or `ci1-merge`. A missing status token, failed
lookup, or malformed response leaves the extra jobs on Namespace, while reserved primary
routing remains available without that token. Forks never reach any ci1 output.
Clippy, compatibility, generated-file checks, TypeScript, isolation and cost
jobs always use existing Namespace capacity. The
aggregate stays on GitHub-hosted capacity regardless of that choice. The required check names
are unchanged; `linux-gate` names both test shards, its supporting stages and the redelivery
canary instead of `needs.*`, because `pick-runner` is skipped whenever ci1 is off.
Priority selection follows the fork boundary, so a fork label cannot reach a self-hosted runner.
The private host controller keeps the priority slot out of the ordinary pool and removes its merge
label while urgent required checks are pending. It also lends the other general runners to
priority work during that interval, so queued ordinary work cannot take the next free slot.
A guard applies labels after every ephemeral registration, before the runner accepts work.
It changes labels without interrupting running jobs; the dedicated merge runner stays available.
This is a snapshot of idle capacity, not an atomic reservation. Two runs that pick at the same
moment can both choose ci1; the later run's jobs then wait for runners on ci1. Each picker allocates
distinct slots within its own run; it never promises one idle worker to both extra jobs.

The switch is the repository variable `CI1_RUNNERS`: unset (the default), `pick-runner` is skipped
and all workload jobs go to Namespace. `on` turns the choice on, and unsetting it turns
it off again without a pull request. `pick-runner` reads the runners with the
`CI1_RUNNERS_READ_TOKEN` secret, a token that may only read the organization's self-hosted runners;
without it ordinary primary and extra jobs go to Namespace. Reserved merge/priority primary
selection needs no organization status token; a failed PR-label lookup retains merge capacity.

On ci1 each runner is ephemeral: it takes one job, runs it as its own user in a fresh work directory
with its own `/tmp`, and nothing the job started outlives it. The runner names a Cargo home in
`CI_LOCAL_CARGO_HOME`; the stage jobs then skip the `actions/cache` restores and the local Nix
cache, use that Cargo home, and Cargo keeps its intermediate build files in a per-runner build
directory, while sccache shares compiled crates between all runners and the Nix store is the
machine's own. The machine's configuration lives in the private network repository.
In GitHub Actions, both Rust dev shells use `scripts/ci-rustc-wrapper`: sccache's response-I/O
fallback is enabled. A separate read-only `sccache --dist-status` preflight is bounded
to 15 seconds (five seconds to finish terminating it). A recognized preflight startup
error or timeout disables caching for the rest of that job with a warning, before any
compiler request is submitted. Signal statuses are preserved. Once compilation starts,
the wrapper executes sccache directly and never retries based on its stderr; the actual
cache/compiler result is authoritative, and cache statistics are optional.
The boundary guard's dependency-free synthetic workspace uses
a fresh Cargo home and no compiler wrappers, stays offline, and prints Cargo stderr on failure.
Cargo builds use the host's four-job limit. Workspace test shards explicitly use eight test
threads, with the host's 14 GiB per-job memory limit. The repository variable `CI1_MIN_IDLE`
can override admission; keep it at four when preserving CPU capacity for reserved lanes.

The local test stage sets `TMPDIR`, `TMP` and `TEMP` to the short `RUNNER_TEMP/t` path on
the runner's memory filesystem. Otherwise `nix develop` chooses disk-backed `/tmp`, making
every test-store commit wait for a disk sync. Stores survive fixture process restarts and are
removed with the ephemeral job; fixture executables retain their target scratch directories.
The stage logs the scratch filesystem. Namespace and the performance jobs retain their existing
temporary storage, and the host's per-job memory limit also covers this test scratch.

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
(`perf.yml`): on relevant `main` pushes and pull requests, nightly on `main`, and on dispatch.
It always uses Namespace, including when Workspace CI picks ci1. It serves a store the size of a busy host's (scale 1, about 240,000 claims) to the
request mix and rates that host's daemon reported in its busiest five-minute window (30 requests a
second: harness events, mailbox pages, claims, desired state, delivery holds, replication rounds,
renewals, status and work reads, and a person's reads), with the reconciler running and 30
concurrent seat event long-polls. Quiet polls have a 31-second budget for their intentional
30-second wait; mailbox WebSockets require authenticated native drivers and are excluded.
The client usage endpoint has a dedicated 0.5-request/second scenario with a 300 ms p99 budget.
Generated usage history contains cumulative response rollups for long-lived standing sessions
with stable attribution, so this scenario exercises many observations per series and period
baselines and totals rather than an empty report or one series per observation.
It fails when
a request's p99 or the daemon's CPU passes its absolute budget, or exceeds twice the worst of
main's last five reports and the corresponding slack below. The relative factor is 2x for both
route p99 and average daemon CPU; every absolute budget still applies independently, including
the 300 ms roster budgets and 2-core CPU ceiling. The wider relative tolerance accommodates
variation on shared runners; a passing historical comparison does not establish a paired effect
or attribute a difference to runner noise. Each run downloads the newest five real reports from successful
main runs' `perf-load-logs` artifacts. PR runs never supply baselines. Relative latency comparisons start once five
main reports exist; until then every path still checks its absolute p99 budget and every request
error fails. CPU compares as soon as one main report exists, because it averages the whole run.
Relative latency tolerates 5 ms of noise, or 50 ms when either path has fewer than 50 samples:
those sparse p99s are effectively observed maxima. This bounded tolerance still catches large
regressions on rare paths. CPU tolerates 0.05 cores. A PR without a main baseline fails as P0;
a main bootstrap may check only absolute budgets and errors.

Performance uses the small `.#perf` Nix shell and the opt-in `perf_load` test target (feature
`perf-load`), which imports the same `daemon_load` and `daemon_bench` modules without compiling
all integration fixtures. Completed main and PR runs retain seven-day build snapshots containing
Cargo outputs, registry/git sources, a local sccache, and a signed Nix binary cache including the
small shell closure. The build recipe includes the lockfiles, shell, manifests and Cargo config.
Generated-store snapshots have a separate exact generator/schema key. Publication requires a
complete real workload report, proving compilation and store generation finished; a missed latency
budget still retains these valid caches. Cancelled and incomplete runs do not publish. Failed
reports never become main baselines. PRs prefer their own published snapshots, then fall back to
trusted main artifacts. A PR snapshot must match the head
repository and branch and a commit in the current PR; it cannot supply main's caches or anyone's
baseline. A rerun checks the previous completed attempt explicitly, since the current run is
in progress, but GitHub may no longer expose that attempt's artifacts. If they are unavailable,
restore another published snapshot of the same PR or main's snapshots. Main snapshots are the durable
fallback; retrying the only PR seed before main has published can still be cold.
Prefer `gh run rerun RUN_ID --job JOB_ID` for retrying only Performance (use the API job ID).
The measured targeted retry retained its preceding artifacts; the full workflow retry did not.
Cargo and sccache validate source changes, preserving each build's real source identity.
Build and store archives download and extract concurrently; the log records each one's timing.
Snapshots also retain the input timestamps from before compilation. Restore those timestamps
only for the exact saved Git SHA with a clean checkout, so a retry does not rebuild and relink
unchanged sources merely because checkout refreshed their timestamps. Changed or dirty sources
still take Cargo's normal validation path; the embedded source revision remains real.
For a clean checkout Performance passes its actual full SHA as `AGENT_SPEC_REVISION`.
The source-identity and shared CLI-version build scripts track that explicit value instead
of unrelated Git index/ref timestamps. The CLI stamp still derives its real revision and
commit time from Git.
Dirty checkouts continue to derive their identity from Git.
PR snapshots also remain for seven days, outside the shared dependency-cache pool.
The compiler-cache server stays alive during cold store generation; publication does not require
stopping it after the workload has finished writing compiler outputs.
Main report artifacts remain for thirty days. `scripts/ci-perf-cache-test` checks provenance,
workload compatibility, report selection and required PR baselines.

These snapshots avoid the repository's shared dependency-cache eviction: on 2026-10-04 the
10 GB pool had already evicted main's just-saved Performance build and stores. A cold PR took
25m21s, including 837s generating stores; a cache-hit PR still took about eleven minutes, including
5m16s compiling the whole integration target and almost three minutes setting up unrelated tools.
The first new-recipe PR passed in 17m51s (4m22s compiling, 584s generating), comparing two real
main reports. Every missing snapshot is flagged P0. A new store recipe generates its source stores on RAM-backed
scratch space; the harness copies them to ordinary runner disk before measuring. `perf-cost`
continues using the Actions cache with the same generator/schema keys.

Run either locally with `TMPDIR=/var/tmp`; `ST_BENCH_DIR` keeps the generated stores between runs:

```sh
cargo test -p st3 --test integration daemon_cost:: -- --nocapture --test-threads 1
ST_LOAD_GATE=1 nix develop .#perf -c cargo test --release -p st3 --features perf-load --test perf_load daemon_load:: -- --nocapture
```

### Cache coverage across workflows

Every job declares its cache coverage. Jobs that select runners, collect checks, run
standard-library guards or consume verified release artifacts have no build cache and say
why. The other jobs report exact hits, lookup-only hits, fallback restores, misses and
failed lookups separately. A missing exact key or failed restore emits a P0 warning in
both the log and the job summary; a skipped archive on ci1 is identified as a persistent
runner store, rather than called an archive hit.

Cargo keys include the lockfiles, workspace manifests and linker configuration. Nix keys
include the flake, Nix expressions and compatibility pins. Both cover macOS as well as
Linux, with isolated Cargo and Nix-cache directories. Genie freshness and isolation have
job-specific caches seeded on main; portable builds use the same pinned Rust cache action as native
releases. Native releases keep the pinned Zig compiler under a stable version/platform key instead of
per-run entries. Matching native snapshots also retain it, so fresh nodes do not fetch it
again. These Cargo builds do not produce Zig object-cache directories; empty cache declarations
would miss on every run and are omitted.

Workspace Cargo and Nix dependency entries are restored on PRs and merge groups. Only
protected main fills missing entries: Linux main upkeep and main macOS runs save them
explicitly after preparing valid outputs. Branch-scoped copies of these large archives
would consume the repository's shared quota and evict the main entries every new PR
needs. Repeated source heads retain their separate exact-source artifact snapshots.
The secondary Linux shard reads the same dependency keys as the primary; snapshots
remain specific to each job, source, platform and build recipe.

Compiled outputs also have three-day artifact snapshots keyed by the actual full source SHA,
job, platform, architecture, build flags and workflow contents. Native snapshots additionally
fingerprint the installed Rust compiler and, on macOS, the Swift compiler and SDK. A fresh
runner restores a compatible completed run's outputs and the original tracked-source
nanosecond timestamps only for that exact clean SHA. Git metadata is never restamped.
Cargo still rebuilds dirty or changed source and embeds the genuine source identity.
The signed Nix cache retains locally built shell tools and their runtime closures, so
a target hit does not rebuild the immutable PTY and collector before Cargo starts.
Upstream-signed compilers and Node stay with their faster public binary caches.
Persistent ci1 runners keep their source checkpoint beside the actual target directory.
A first build of a new source warns that no snapshot exists and restores dependency caches;
a repeat reuses its snapshot without consuming the shared dependency-cache quota.

Native releases keep the pinned PTY checkout at a stable Cargo-home path and retain the
macOS speech app only with a matching native snapshot. Portable releases keep the immutable
workflow helpers before checking out the accepted source, so earlier accepted sources can
use the cache policy without weakening source, binary or publishing verification.

Main upkeep removes cache entries belonging to closed PRs and redundant old Zig snapshots.
It retains open PR entries, current main recipes and one legacy Zig snapshot per platform
while the new cache is seeded. This maintains the shared cache quota without removing
active work. Native Nix verification probes real output paths before building and still
proves its repeat with downloads and builds disabled. Performance retains its separate
durable snapshots and successful-main-only baselines.

The macOS workflow was manually disabled when this audit ran. Its cache configuration is
maintained without changing that repository setting. Portable publishing permissions and
accepted-source verification remain in place.

## Required mail redelivery canaries

Alongside the two Linux test shards, `scripts/ci-mail-redelivery-canaries` requires twenty named,
unignored regressions: boot/reconnect mailbox suppression and recent unoffered recovery for
Claude, Codex, OpenCode, Pi, and OMP; each harness's native suspend/resume canary with hour-old
mail held and recent unoffered mail consumed exactly once; legacy polling recovery through the
current offer's receipt sequence; and delivered-but-unread retention across native channel
restart. Claude's missing-channel cases also require automatic recovery, attachment during
recheck without replacing the seat, and durable parking after three failed replacements.
The mailbox cases seed hour-old sent mail and recent staged and delivered-but-unread
mail, prove zero historical offers, preserve explicit mailbox access, and recover an in-flight
send after a daemon restart with exactly one receipt pair. Every selected test runs with zero retries.

The script fails if any required test is missing, ignored, or filtered out. The named
`mail-redelivery-canaries` check executes the script on its own Namespace 8x16 runner; a skipped
or failed job cannot pass.
`linux-gate` requires it, so this protection applies to pull requests and merge groups. It
restores the main-seeded Linux test Cargo and Nix caches, prepares the same fixtures and rendered
hooks, and retains its own logs, timing and failure evidence. It never uses the native priority
or merge lanes, and neither test shard waits for it before starting its own suite.
Apply the fifth live ruleset check after this workflow has passed on main.

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
| `nix.yml` | Build the package and all native checks on tags, relevant trusted PRs, dispatch, and daily at 01:17 UTC. Use ci1's persistent Nix store and GC roots; verify a repeat with downloads and builds disabled. No Actions cache or FlakeHub dependency. Forks use the public Linux gate. |
| `release-smalltalk.yml` | Preserve tag/dispatch/fork-PR triggers, target packaging, source verification and publishing permissions. Move Linux and ARM macOS runners to Namespace. Also run on every `main` push, keeping the archives as 7-day artifacts, so release breakage fails on `main` (not required for merging). |
| `release-daily.yml` | New. Once a day, publish the archives of the newest successful `main` release run when `main` changed since the last release; see [binary releases](st3/binary-releases.md#daily-releases). |
| `release-portable.yml` | Preserve dispatch inputs, accepted-source verification, packaging, publishing and fresh-download execution proof. Move Linux to Namespace. |
| `public-repo.yml` | Preserve the guard and its tests on all PRs and main pushes; move Linux to Namespace. |

The content guard still rejects real machine/home identities and private fleet configuration
references. Release workflows remain separate from the required gate; neither package
verification nor the Nix package/check graph is made redundant by workspace nextest.

## Merge queue

`main` lands through GitHub's merge queue. Add a ready pull request to it with

```sh
gh pr merge NUMBER --auto
```

The queue tests the pull request on top of the current `main` and the entries ahead of it with
`linux-gate`, `isolation-vm`, `genie-freshness`, `typescript-client`, and
`mail-redelivery-canaries` (these run on the `merge_group` event; see the
trigger in `fleet.yml.genie.ts`) and merges it with a merge commit when they pass. The pull
request does not need to be rebased onto the latest `main` first. A draft cannot be queued. If a
queued check fails, the entry leaves the queue and the pull request page says why: fix it and
queue it again. The merge train (`st lanes join smalltalk`) is retired.

The ruleset (`.github/repo-settings.json`, generated from `repo-settings.json.genie.ts`, applied
by an administrator and never by CI) requires the five checks from GitHub Actions with an empty
bypass list, keeps the pull-request, deletion and force-push protections, and configures the queue:
merge method MERGE, a proposed three entries build at once (see [Merge overflow and daily Namespace minutes](#merge-overflow-and-daily-namespace-minutes)),
up to five merge together, and a check that
never reports fails its entry after 60 minutes. Repository settings enable native auto-merge and
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
With the current 8x16 stage runners, a merge-queue Workspace CI group initially starts four
8-vCPU/16-GiB stage jobs and two 8-vCPU/16-GiB profile jobs: 48 vCPUs and 96 GiB at peak.
The TypeScript client job follows generator freshness and reuses its runner slot.
PR runs also start `perf-cost`, taking their initial peak to 56 vCPUs and 112 GiB. Main upkeep
runs that check separately; its cache-fill jobs normally finish after their lookup-only probes.
Five complete merge-queue groups need 240 vCPUs and 480 GiB, within the Linux pool limit;
`max_entries_to_build` was 5 at this historical measurement; current capacity policy appears in [Merge overflow and daily Namespace minutes](#merge-overflow-and-daily-namespace-minutes).
PRs, main pushes and other workloads share that capacity; Namespace queues jobs until resources
are available. The `linux-gate` aggregate starts after the four stage jobs finish, so it does
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
job publishes platform limits and current usage for `workflow_dispatch` to the job
summary and retains the `namespace-capacity` artifact. It reports no workspace or account identity.
See Namespace's [resource limits](https://namespace.so/docs/architecture/compute/resource-limits)
and [profile concurrency controls](https://namespace.so/docs/solutions/github-actions/runner-controls/concurrent-runners)
for the scheduler's limits, and GitHub's [merge queue settings](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)
for the distinction between build concurrency and merge batch size.

## Superseded merge groups

Push concurrency in Main upkeep, Performance and the public guard is scoped to protected main.
A new main push cancels obsolete automatic work; its upkeep still probes and fills the current
main's exact cache entries. PR groups use the PR number, rather than its head SHA, so a new head
replaces the old one. Manual pinned validations are never grouped with PR or push runs. Old
immutable workflow revisions can remain queued: before cancelling a backlog entry, compare its
event, source and PR state with current main or the PR's latest head, retain its original timing
and cancellation, and leave manual controls and merge groups to their owners.

The `namespace-capacity` job also watches `merge_group` events on a GitHub-hosted runner,
independent of ci1 and Namespace availability. It polls the group's ref and required check statuses
every 15 seconds. A missing ref or a changed SHA on two consecutive successful lookups force-cancels
its own workflow, including queued Linux jobs and always-run summaries. Lookup failures retain work.
Once all five required checks finish, the watcher exits normally; a ref removed by a successful
merge therefore retains the completed workflow result used by main upkeep. It executes embedded
workflow code without checking out queued PR code. Manual capacity reports still use Namespace.

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

During an outage, `CI_OUTAGE_FAST_QUEUE=on` skips optional Nix-cache saves and same-source build-snapshot publication on merge-group runs. Required checks, cache restores, the producer test archive, logs, cache coverage and PR/main cache publication continue. Set the variable back to `off` when Speed ends the outage; an unset variable also preserves normal publication. The earlier six-build incident guidance and automatic restore-to-two instruction are superseded by CI-speed-owned capacity tuning. The current source proposes three with Namespace overflow; the initial live trial returned to two after a measured PR wait exceeded five minutes. Preserve all other live ruleset fields when changing admission.

## Merge overflow and daily Namespace minutes

CI-speed owns live CI variables and queue capacity. Start with `max_entries_to_build=3`,
merge sizes 1–5 and a five-minute batch wait; live HEADGREEN remains the incident's
existing strategy. The generated ruleset's normal ALLGREEN strategy is unchanged.
Change only the build-count field when tuning the live ruleset. This is capacity
configuration, not proof of CI p90 <20 minutes, runner wait <5 minutes or queue-to-merge
p90 <45 minutes.

With `CI_MERGE_CI1` unset/off, the picker admits an entire group to `ci1-merge`
only when five distinct online idle merge workers are available and, after borrowing
mixed workers, two general workers remain for PRs. Priority-only workers are excluded.
Otherwise all group workload jobs use Namespace. Admission is a snapshot, not an
atomic reservation across simultaneous pickers. Missing/invalid status also overflows.
`CI_MERGE_CI1=on` retains the explicit forced-local switch for supported incidents.
PR/fork/priority routing is unchanged. No job is migrated after it starts.

`CI_MERGE_NAMESPACE_PROFILE` may name an existing, verified Linux profile; unset
uses the existing 8x16 stage labels and Linux profile for supporting jobs, including
KVM. Profile controls and run affinity remain inline. The initial live choice is
unset: the shared `linux-x86-64` profile historically limited parallel runners, so
funnelling all large stages into it can queue them despite workspace headroom.
Three groups can request roughly 144–168 vCPU / 288–336 GiB during overlap on
Namespace, below the held 320-vCPU / 640-GiB workspace limit, with capacity shared
by PRs and other work. Actual starts/waits decide later tuning, not those upper
bounds alone. A dedicated capped profile requires an actual administrator receipt;
this PR does not claim one was provisioned.

`Namespace usage` runs at 03:05 UTC on GitHub-hosted capacity and reports the previous
UTC day's observed Namespace job execution minutes, split by event (merge_group,
pull_request and other events). Cos can read the job summary and its 30-day JSON
artifact for the morning cost check. The report has a 120-minute timeout, but request
count is bounded separately: at most 4,000 GitHub API requests. It reuses the existing
`CI1_RUNNERS_READ_TOKEN` from the main-only picker, without a new secret. Standard
PAT/installation tokens have a 5,000-request hourly primary limit; the 1,000/hour
job-token fallback cannot cover this repository's measured volume. The reporter
checks actual response rate-limit headers, refuses a limit below its budget, and
records the observed limit, lowest remaining count and actual requests in JSON.
The collector stops at 500 remaining requests to reserve shared quota for runner
admission. Shared credential use can still exhaust available quota and must remain visible.
`python3 scripts/ci-namespace-usage --date YYYY-MM-DD --output usage.json` also
provides an on-demand report. It queries eight UTC creation days: the reporting
day and seven prior days, so reruns of older runs are outside coverage. Each day
is queried separately, recursively splitting saturated time ranges until each
query is below GitHub's 1,000-result cap. A saturated one-second interval or
changing search that reaches the cap while paging refuses completeness. Within this window it queries all attempts, includes
failed/cancelled execution, deduplicates job IDs and clips executions across midnight.
Every day total is explicitly a **daily lower bound**, never a complete-day or
invoice total. “Complete window evidence” means only that this creation window
was collected without known evidence gaps; it does not cover older-run reruns.
The job identity must be an actual `nsc-runner-*`, rather than just a planned
Namespace label. Queued/skipped jobs and queue cancellations with no runner or
started step add no execution minutes. A started step with missing/ambiguous
Namespace runner identity, or an actual Namespace runner with missing timestamps,
makes summary/JSON totals explicitly partial and fails the report. Known local
and GitHub-hosted execution stays excluded. API errors, exhausted request budgets
or pagination limits retain collected JSON as partial lower-bound evidence before
failing, rather than declaring zero usage. Pagination is not an atomic history snapshot.
Totals are operational execution minutes, not invoice dollars or billable unit
minutes: provisioning before the first job step and deleted GitHub history are
outside this method. Use Namespace's billing view for an invoice; the report
preserves raw job IDs/timestamps for reconciliation.

Intake's read-only day counts for October 2–8 were 949, 1,222, 1,227, 1,437,
1,126, 1,761 and 1,699 runs. The last day alone requires at least 1,699 jobs
requests plus run-list pages; this invalidated the earlier 800-request/job-token
design. The bounded 4,000-call design and earlier schedule allow this scale, but
actual call count, credential capability and natural completion remain execution
evidence, not a guaranteed bound on future repository volume.
The initial historical collector ended naturally after 37m05s, scanning 14,135
retained run records and 1,761 relevant runs. That requires at least 1,903 API
calls (142 run-list pages plus at least one jobs page per relevant run); its exact
request count was not instrumented. Its older identity gap makes its 38,979.28
observed job minutes a lower bound, not current collector qualification. The new
collector records exact request/rate evidence for its own natural outcome.

The reporter makes only read-only GET requests to Actions run/job endpoints. The
existing secret must permit those reads in addition to the picker's organization
runner-list access; workflow job-token permissions do not restrict a PAT's actual
scopes. Source alone cannot confirm the installed secret's scope or capability.
A missing permission returns partial evidence rather than a complete total.
See GitHub's [workflow-run search limits](https://docs.github.com/en/rest/actions/workflow-runs#list-workflow-runs-for-a-repository)
and [REST rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api).
Each response must have fresh valid limit and remaining headers; earlier values
never substitute for missing current quota evidence.

One pinned corrected local-auth report ended naturally with exit 1 after 207.77s:
234 attempted API calls, observed limit 5,000, minimum remaining 499, and the
explicit reserve-reached error. It retained 4,836.67 observed minutes as partial
window evidence. This confirms the quota-reserve behavior with local authentication,
not the scheduled credential, full window, or a successful daily total. No automatic
retry follows quota exhaustion.

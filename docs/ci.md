# CI operations

The fleet's CI mission declarations and trusted runner scripts are kept with that fleet's private
machine configuration, outside this repository, as `smalltalk-ci-*.kdl` and `smalltalk-ci*.sh`. The standing `st` mission observes
same-repository pull request heads and pushes to `main`. Each run checks the exact observed
commit. A pull request run merges that commit with the latest `main` in a temporary checkout
before running `cargo test --workspace --locked` and
`cargo clippy --workspace --all-targets --locked`. Linux also checks generated clients and
runs the fleet compatibility test against the pinned older st3 baseline. The normal Linux test
suite also runs the token-free two-node messaging fault matrix: daemon restart, binary replacement,
short and two-minute partitions, receiver downtime, provider restart, channel death, an actual
historical channel and repeated rejected handoffs. It requires one native handoff and a graph
read within ten seconds of recovery. The historical channel build is pinned separately in
`.github/messaging-compat-baseline.json` and cached by Nix. See
[the eval contract](../evals/st3/messaging-faults/README.md).

The workspace tests and the public GitHub Actions job run `scripts/check-public-repo`. It
rejects real host names, personal home paths, unlisted person IDs, internal fleet agent IDs,
and references to private fleet configuration repositories.

On Linux, the workspace test build runs first and alone, without debug information. Then the
tests, Clippy, the generated client check and the fleet compatibility test run side by side, and
the run fails if any of them fails. The tests run under nextest, 8 at a time, with the tests
that take a minute or more started first. A failed test is retried twice, 30 seconds apart, and
one that passes on a retry is reported as flaky rather than failing the run.

The repository's nextest configuration starts the full messaging fault matrix before shorter
tests. Its retries need most of the 25-minute test limit, so starting it at the end can cut off
the last attempt even when the other tests pass. Recovery deadlines and assertions stay the same.

st2's catalog, supervisor and end-to-end tests cover st2 code that st3 does not use: the
`agent_author`, `catalog*`, `eval_run`, `resync` and `resource_profile_supervisor` modules and
the `catalog_*`, `nomad_survival`, `event_e2e`, `eval_run_e2e`, `resync*`,
`supervisor_auto_archive` and `resource_profile_supervisor_e2e` test files. A pull request skips
them when every path it changes is st3's own code, clients or documents, one of the st2 modules
st3 uses (driver, channels, hooks, messages, harness state and sessions), or another st2 test
file. Any other change runs them, including `Cargo.lock`, the root `Cargo.toml`, shared test
support and the shared crates. A run that skips them says "st2 catalog and supervisor tests not
needed" in its `st/ci` description. `main` runs them once a day: the first `main` run after a day
without a passing one.

`st/ci` is the Linux result from the CI machine and is the pull request merge check.
`st/ci-macos` runs on the macOS CI machine for `main` commits. To request a macOS run on a
pull request, add the `macos-ci` label; the run starts after Linux succeeds.
Linux runs use two host-local Cargo target lanes, while macOS reuses one target
directory. Each run isolates `HOME` and XDG directories. Forked pull requests
are excluded before any code from them runs on these machines. GitHub Actions handles tags
and forks.

macOS builds use a stable checkout path and a separate Cargo cache with debug information and
incremental compilation disabled. Darwin's unpacked debug objects can otherwise accumulate
beside test executables, slowing dependency lookup and native filesystem watcher startup. The
runner clears the debug cache when its dependency directory exceeds 20,000 files. The Codex
control tests remain enabled on macOS, and the test command retains its 25-minute limit.

The macOS summary includes the checked head, each stage's elapsed seconds, and the overall
result. Stage logs have matching `.time` files recording elapsed seconds and command exit status.

## Merge rule

The `main` ruleset refuses to merge a pull request into `main` unless `st/ci` succeeded on the
pull request's exact head and that head is up to date with `main`. Nobody can bypass it.
Merging one pull request therefore makes every other open pull request behind `main`.

The [merge train](#merge-train) does this for you, one pull request at a time. To merge by hand,
update the branch with `main` (`gh pr update-branch NUMBER`, or merge `main` yourself), wait for
`st/ci` on the new head, and merge while the pull request is still not behind.

- A head that is behind `main` is refused even when `st/ci` succeeded on it. `gh pr merge` says
  "the head branch is not up to date with the base branch"; the REST API says `Required status
  check "st/ci" is expected`, because a status on an out-of-date head does not count. A head
  without a passing `st/ci` gets the same status-check message.
- A failed run, or a head whose run was skipped, is re-run by pushing a new head; merging
  `main` into the branch is enough. A closed pull request and a draft get no `st/ci` status. The
  observed head of a reopened or ready pull request is only run again when it changes.
- A fork's code never runs on these machines. So the rule does not block forks, `st/ci` posts
  one success on a fork's head that says it does not run for forks. GitHub Actions is what checks
  the fork.
- If the standing CI seat stops, no pull request can merge until CI runs again; change the
  `main` ruleset only for that.

## Merge train

The merge train is a [lane](st3/lanes.md) named `smalltalk`, owned by the
merge-train mission (`smalltalk-train.kdl` and `smalltalk-train.sh` in the same
missions directory). When a pull request is ready to merge, join it:

```sh
st lanes join smalltalk NUMBER
st lanes show smalltalk
```

An agent joins as its own seat; a person joins with `--as person/NAME` or the configured person.
The train's driver works through the lane front first:

1. It marks each pull request. A draft, a pull request that does not merge cleanly with `main`,
   and a head without a passing `st/ci` are `waiting`. A pull request from outside the fleet is
   `held` until the lane's approver runs `st lanes approve smalltalk NUMBER`. A pull request
   whose own head passed `st/ci` is `ready`. A closed, merged, or forked pull request leaves.
2. It takes the first `ready` pull request, and only that one. If its head is behind `main`, it
   merges `main` into the branch on GitHub (the same as `gh pr update-branch`), and the new
   head's `st/ci` run waits in the short queue that `main` runs use.
3. It merges when `st/ci` passed on that exact head and the head still contains `main`. When
   `main` moved first, `st/ci` failed, or someone pushed during the run, the pull request goes
   to the back of the lane, and the driver takes the next one.

A pull request stays in the lane until it merges or you run `st lanes leave smalltalk NUMBER`.
After a failure, push a fix; the pull request becomes `ready` again when `st/ci` passes on the
new head. The train pushes a merge of `main` onto your branch, so pull before you push again.

Other pull requests keep getting `st/ci` on their own heads. Merging by hand still works, and
the train treats a pull request merged that way as done; a hand merge while the train is
testing its car only sends that car to the back.

## Inspect a failure

The commit status description includes the mission run ID. On example-linux:

```sh
st missions show mission-run/RUN-ID
st trace show mission-run/RUN-ID
```

The Linux checkout, summary and test logs are under
`~/.local/state/st3/smalltalk-ci/runs/RUN-ID/`. Each step writes `logs/STEP.log` and
`logs/STEP.time`; the steps are `components`, `hooks`, `build`, `test`, `clippy`, `codegen` and
`fleet-compat`. On macOS, the corresponding files are in the `macos/`
subdirectory. A failed command's stderr is in its log. The summary records whether st2's
catalog and supervisor tests ran and why (`st2_rest=`), the failed steps (`failed=`), each step's
seconds (`stages=`), the elapsed time and the final result. Use the run's source claim and
head SHA to distinguish a current failure from a run superseded by a newer
commit.

The `st/ci` status is posted as pending when the Linux check starts and as
success or failure when it ends. Scheduled `st/ci-macos` runs follow the same sequence.
If a run never reaches a final status, inspect its gate operation and the
standing mission with `st missions show`.

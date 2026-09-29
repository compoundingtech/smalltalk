# CI operations

The fleet CI definitions and trusted scripts live in
[`myobie/st3-network/missions`](https://github.com/myobie/st3-network/tree/main/missions)
as `smalltalk-ci-*.kdl` and `smalltalk-ci*.sh`. The standing `st` mission observes
same-repository pull request heads and pushes to `main`. Each run checks the exact observed
commit. A pull request run merges that commit with the latest `main` in a temporary checkout
before running `cargo test --workspace --locked` and
`cargo clippy --workspace --all-targets --locked`. Linux also checks generated clients and
runs the fleet compatibility test against the pinned older st3 baseline.

On Linux, the workspace test build runs first and alone. Then the tests (under nextest, 8 at a
time, longest first), Clippy, the generated client check and the fleet compatibility test run
side by side, and the run fails if any of them fails.

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

`st/ci` is the Linux result from hetz and is the pull request merge check.
`st/ci-macos` runs on Silber for `main` commits. To request a macOS run on a
pull request, add the `macos-ci` label; the run starts after Linux succeeds.
Linux runs use three host-local Cargo target lanes, while Silber reuses one target
directory. Each run isolates `HOME` and XDG directories. Forked pull requests
are excluded before any code from them runs on these machines. GitHub Actions handles tags
and forks.

## Merge rule

The `main` ruleset refuses to merge a pull request into `main` unless `st/ci` succeeded on the
pull request's exact head and that head is up to date with `main`. Nobody can bypass it.
Merging one pull request therefore makes every other open pull request behind `main`.

To merge, update the branch with `main` (`gh pr update-branch NUMBER`, or merge `main` yourself),
wait for `st/ci` on the new head, and merge while the pull request is still not behind. Merge
one pull request at a time.

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
- `st/ci` runs as the `agent/fleet/smalltalk-ci` seat. If it stops, no pull request can merge
  until CI runs again; change the `main` ruleset only for that.

## Inspect a failure

The commit status description includes the mission run ID. On hetz:

```sh
st missions show mission-run/RUN-ID
st trace show mission-run/RUN-ID
```

The Linux checkout, summary and test logs are under
`~/.local/state/st3/smalltalk-ci/runs/RUN-ID/`. Each step writes `logs/STEP.log` and
`logs/STEP.time`; the steps are `components`, `hooks`, `build`, `test`, `clippy`, `codegen` and
`fleet-compat`. On Silber, the corresponding files are in the `macos/`
subdirectory. A failed command's stderr is in its log. The summary records whether st2's
catalog and supervisor tests ran and why (`st2_rest=`), the failed steps (`failed=`), each step's
seconds (`stages=`), the elapsed time and the final result. Use the run's source claim and
head SHA to distinguish a current failure from a run superseded by a newer
commit.

The `st/ci` status is posted as pending when the Linux check starts and as
success or failure when it ends. Scheduled `st/ci-macos` runs follow the same sequence.
If a run never reaches a final status, inspect its gate operation and the
standing mission with `st missions show`.

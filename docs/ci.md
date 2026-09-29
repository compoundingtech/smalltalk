# CI operations

The fleet CI definitions and trusted scripts live in
[`myobie/st3-network/missions`](https://github.com/myobie/st3-network/tree/main/missions)
as `smalltalk-ci-*.kdl` and `smalltalk-ci*.sh`. The standing `st` mission observes
same-repository pull request heads and pushes to `main`. Each run checks the exact observed
commit. A pull request run merges that commit with the latest `main` in a temporary checkout
before running `cargo test --workspace --locked` and
`cargo clippy --workspace --all-targets --locked`. Linux also checks generated clients and
runs the fleet compatibility test against the pinned older st3 baseline.

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
- `st/ci` runs as the `agent/fleet/smalltalk-ci` seat. If it stops, no pull request can merge
  until CI runs again; change the `main` ruleset only for that.

## Merge train

The merge train is a [lane](st3/lanes.md) named `smalltalk`, owned by the
`fleet/smalltalk/train` mission (`smalltalk-train.kdl` and `smalltalk-train.sh` in the same
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

The commit status description includes the mission run ID. On hetz:

```sh
st missions show mission-run/fleet/smalltalk/ci/run/RUN-ID
st trace show mission-run/fleet/smalltalk/ci/run/RUN-ID
```

The Linux checkout, summary and test logs are under
`~/.local/state/st3/smalltalk-ci/runs/RUN-ID/`; the relevant files are
`summary`, `logs/test.log`, `logs/test.time`, `logs/clippy.log`, and
`logs/clippy.time`. On Silber, the corresponding files are in the `macos/`
subdirectory. A failed command's stderr is in its log. The summary records
the measured elapsed time and final result. Use the run's source claim and
head SHA to distinguish a current failure from a run superseded by a newer
commit.

The `st/ci` status is posted as pending when the Linux check starts and as
success or failure when it ends. Scheduled `st/ci-macos` runs follow the same sequence.
If a run never reaches a final status, inspect its gate operation and the
standing mission with `st missions show`.

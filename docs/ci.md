# CI operations

The fleet CI definitions and trusted scripts live in
[`myobie/st3-network/missions`](https://github.com/myobie/st3-network/tree/main/missions)
as `smalltalk-ci-*.kdl` and `smalltalk-ci*.sh`. The standing `st` mission observes
same-repository pull request heads and pushes to `main`. Each run checks the exact observed
commit. A pull request run merges that commit with the latest `main` in a temporary checkout
before running `cargo test --workspace --locked` and
`cargo clippy --workspace --all-targets --locked`.

`st/ci` is the Linux result from hetz. `st/ci-macos` is the Silber result and starts
after the Linux result, so a slow macOS build does not delay `st/ci`. Both reuse a host-local Cargo
target directory and isolate `HOME` and XDG directories for each run. Forked pull requests
are excluded before any code from them runs on these machines. GitHub Actions handles tags
and forks.

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
success or failure when it ends. `st/ci-macos` follows the same sequence.
If a run never reaches a final status, inspect its gate operation and the
standing mission with `st missions show`.

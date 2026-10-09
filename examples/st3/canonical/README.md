# Canonical missions

Three reusable missions that show what a mission is and that anyone can run. Smalltalk's first
agent, the Smalltalk Assistant, offers them during onboarding and applies one for you if you say
yes. Setup stores each file as `doc/st/canonical/NAME`, so they are also available on a machine
without this repository.

| File | What it does | Needs |
| --- | --- | --- |
| [`weekly-session-review.kdl`](weekly-session-review.kdl) | Reads last week's agent sessions, finds habits to break and tools or concepts worth learning, stores one report. Read-only. | Nothing but a seat |
| [`weekly-schedule.kdl`](weekly-schedule.kdl) | Starts that review every Monday morning. | The review, published first |
| [`review-pull-request.kdl`](review-pull-request.kdl) | Reads one pull request, runs what it can in a scratch checkout, stores a review. Never comments on or merges it. | Read access to the pull request's repository |

Only the pull request review involves GitHub. [`seats/reviewer.kdl`](seats/reviewer.kdl) declares the seat by hand.

## Run one

The work runs on a seat named `agent/st/reviewer`. Make it once, with the harness you use:

```sh
st agents new st/reviewer --harness claude --workspace "$HOME/st/agents/st-reviewer"
st apply weekly-session-review.kdl --as person/NAME
st missions start example/canonical/weekly-session-review \
  --id example/canonical/weekly-session-review/first --workspace "$PWD" --as person/NAME
```

Replace `person/NAME` with your configured person. Each file's header has the exact commands.
Check a file without publishing it: `st apply FILE --dry-run --check --as person/NAME`. Preview
reports `missing eligible agent agent/st/reviewer` until that seat exists.

## What keeps the weekly review from leaking

Sessions can hold secrets. The mission tells the reviewer to describe a pattern and never quote a
credential, and its `report` step carries a gate that refuses a `report.md` which looks like it
contains an access key, a token, a private key block or a `password=` style assignment. The gate
answers "not yet" until the report is clean; if `grep` itself cannot run it answers "broken", so a
missing tool is never mistaken for a clean report. `crates/st3/tests/canonical_missions.rs` runs
the gate against the fixtures in [`fixtures/`](fixtures/), which hold invented secrets only.

## Fill in the schedule

`weekly-schedule.kdl` names the weekly review by its revision, and a revision belongs to the
machine that published it, so the file carries a zero placeholder. Apply the review, read its
revision with `st --json missions ls --all` (the item whose id is
`mission/example/canonical/weekly-session-review`, field `mission_revision`), replace the zeros
with it, then apply the schedule. The Assistant does this for you.

All names, paths and people are invented. Replace them before you rely on a file.

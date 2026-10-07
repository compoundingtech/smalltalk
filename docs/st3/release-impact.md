# Release upgrade impact

Build every main commit and keep deployments moving. Public releases default to the daily
schedule; operations may cut an extra release for an urgent fix or a build people need.
Users choose when to install a pinned release. Every publication path uses the same
source-pinned Upgrade impact section before the change list.

## Authors and reviewers

Commit one uniquely named `release-notes/NAME.json` with every PR or direct change.
Copy [the tooling fragment](../../release-notes/upgrade-impact-tooling.json) and replace its
summary and classifications. Each of these fields requires both `status` and `detail`:

| Field | Describe |
| --- | --- |
| `replay` | Full/partial/no replay, affected versions or store state, trigger evidence, and whether the API waits for recovery. A rules bump alone does not prove replay. |
| `schema` | Database changes, including new tables/indexes/triggers without a version bump, and forward-only migrations. |
| `rules` | Checkpoint rules/digests, coordinated member upgrades, and mixed-build effects. |
| `compatibility` | Client/protocol capabilities, pairing scopes, provider admission and tested or unverified version pairs. |
| `downtime` | Expected interruption; distinguish restart-to-API from restart-to-health and estimates from observations. |
| `manual` | Affected existing installations, steps or pinned guide links, preserved identity/state, and verification. |
| `recovery` | Supported swap-back conditions or roll-forward instructions; reinstalling binaries does not undo migrations. |

Use `none` for an explicitly reviewed absence of additional impact, `changed` for known
impact, or `unknown` with a concrete explanation of what is unresolved and what users should
do. Omission is invalid. Unknown is visible in notes, never converted into none.

The fast `upgrade-impact` check runs on every PR and merge group using Python and Git, without
Nix or compilation, and feeds the existing required `linux-gate`. PRs numbered **above #1661**
must add a fresh valid fragment, including documentation-only PRs. PRs #1661 and below without
notes retain their prior merge policy at adoption; publication still requires their full
classifications, so authors should add notes even to those older PRs. The boundary is applied
to each PR in a queue group, not just its head ref. Supplied metadata must be valid and
include matching schema/rules transitions. New PRs missing metadata cannot merge. The check uses the effective merge tree and
immutable event base; it does not require unrelated unreleased main changes to be backfilled.
Existing fragments are immutable: corrections, later measurements and backfills use a new file.

Reviewers check the metadata against the whole change. Labels can draw attention to an impact,
but do not replace the committed details.

For a schema/rules version change, include integer pairs in `transitions`, for example:

```json
"rules": {
  "status": "changed",
  "detail": "Upgrade all members in a coordinated window; mixed rules stall new sealing.",
  "transitions": [[10, 11]]
}
```

The publication validator checks each first-parent source change's schema/rules versions
against matching classified transitions. Include intermediate transitions as well as the
endpoints; notes retain every fragment rather than collapsing to the newest version. An
unchanged numeric schema version does not mean no database changes or safe rollback.

Reproduce the PR check against an effective merge tree (a PR head merged with its base):

```sh
python3 scripts/check-release-impact --base BASE_COMMIT --source MERGE_COMMIT --pr-number PR_NUMBER
```

Without an event identity (`--pr-number` or `--queue-ref`), the local check is strict and
applies no adoption exemption. The publication renderer never applies this PR boundary.

## Operations

The renderer reads fragments and the installation guide from the exact candidate commit,
never uncommitted files or the publication checkout's newer guide. A newly added
fragment classifies its integrating first-parent commit. Merge commits include their branch
changes; direct commits require metadata too. Final fragment versions are authoritative.
Keep older fragments so their impact is available when releasing a wider source interval.
After publication, leave those fragments untouched; use a fresh fragment for later
observations or corrections, explicitly describing the affected source or starting versions.

To classify a previously merged change, commit a reviewed fragment with an optional
`"commits": ["FULL_40_CHARACTER_SOURCE_HASH"]` list. Use the actual full hashes printed by
the validator; a later fragment can cover several reviewed changes. It also classifies its
own integrating commit. Do not add broad none classifications without inspecting those
changes. On rollout, backfill **every unclassified commit since the previous public release**;
there is no silent historical exemption.

Add measured observations under `downtime` where available:

```json
"downtime": {
  "status": "unknown",
  "detail": "Other store sizes have not been measured; allow time for recovery.",
  "observations": [{
    "source": "0123456789abcdef0123456789abcdef01234567",
    "platform": "Linux x86_64 on an NVMe workstation",
    "graph_size": "50000 claims",
    "method": "Service restart timestamp to first capabilities HTTP 200",
    "api_seconds": 501,
    "health_seconds": null
  }]
}
```

Use `null` for an unmeasured milestone. Supply exact source, platform/host context, graph size
and method; observations are not a downtime guarantee. Keep real fleet identities and raw
private measurements outside the public repository.

Preview just the notes without downloading, building, publishing or restarting:

```sh
python3 scripts/release_notes.py --previous v0.3.15 --source COMMIT --tag v0.3.16
```

Without `--previous`, the CLI uses the latest public GitHub release. Run from a checkout with
the relevant source and tags. `--output FILE --manifest FILE` writes the same notes and
`UPGRADE-IMPACT.json` report used by publication. Every source in the interval must be
classified, and schema/rules changes must have matching metadata; otherwise the command
fails with the missing sources. Explicit unknowns remain reviewable in the report.

`scripts/release-smalltalk-daily --dry-run` also verifies the existing archives and prints
the same notes. Manual dispatch uses that same script. Tag publication invokes the same
renderer before creating a GitHub draft. Both paths publish the impact report alongside
the existing exact-source manifest/checksums and refuse publication when classifications
are incomplete. Main builds continue; the PR gate catches missing notes before merge.
No full soak is added per routine release.
The existing friend-ready verification requirements remain separate.

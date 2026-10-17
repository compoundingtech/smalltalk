# A merge queue that lives in git (prototype design)

Status: prototype only. Off by default, wired to nothing, not a product decision. The tool
and its tests are in `tests/git-queue/` (`gq`, `test_gq.py`, `demo.sh`).

**The idea.** Branches are changes in flight. A *bundle* is what you offer the queue to pull
in. The queue is anything that can run git: it looks at the bundles that are ready, takes one,
puts it on `main`, and moves on. It can pull from GitHub, from Nathan's machine, or from
Johannes's machine. It needs no GitHub API, no webhook, no lock server, and nothing added to
commits. A change is identified by its **tree hash** (the content). If you rebase to reformat
it, the content changes, so it is simply a new bundle.

## 1. What a bundle is

A bundle is a ref under `refs/queue/ready/<tree>/<publisher>`, where `<tree>` is the tree
hash of the tip commit. It takes one of two forms:

- **Commit form.** The ref points straight at the tip commit. The queue lands every commit on
  it that `main` does not have.
- **Range form.** The ref points at a tiny blob holding `TIP` or `BASE..TIP`. This names a
  specific set of commits without pointing at them, so the publisher also pushes
  `refs/queue/keep/<tree>/<publisher>` at the tip to keep those commits alive.

What git allows, checked on a scratch repository (and covered by tests):

| Ref points at | Under `refs/heads/` | Under `refs/queue/` (or any other namespace) |
| --- | --- | --- |
| commit | yes | yes |
| blob | **no**: "trying to write non-commit object to branch" | yes: `update-ref`, `push`, `ls-remote`, `fetch`, `cat-file` all work, and the blob survives `gc` on the remote |
| tree | no | yes |

Two details that matter:

- A push of a value the ref already has is a silent no-op ("Everything up-to-date"). That is
  harmless for bundles, but it means a *claim* must carry a unique value (it has a random
  nonce), or two claimers could both think they won.
- **GitHub: verified** on 2026-10-14, on a throwaway private repository under the myobie
  account (results in the section "GitHub result" below). Blob-pointing refs under
  `refs/queue/` are accepted.

## 2. The advertisement: how the queue learns what is ready

| | Plain refs (**recommended**) | A directory of files in a branch | Notes |
| --- | --- | --- | --- |
| Shape | one ref per bundle under `refs/queue/` | `queue` branch with `ready/<tree>/<publisher>` files | `refs/notes/queue` notes on tip commits |
| Reading | one `git fetch` or `ls-remote`; no checkout | fetch and read the tree | fetch, `git notes list` |
| Two publishers at once | never conflict, because they write different refs | the branch is one hot ref, so every concurrent publish is a non-fast-forward and must rebase and retry | same: one shared notes ref, merged on conflict |
| History | none; withdrawing is deleting a ref | grows forever; branch needs squashing | grows |
| Human-readable on GitHub | no (not in the branch list) | yes | no |
| Cost | ref clutter on the server; `gq ls` is the viewer | retry loop; an ordering and cleanup policy | merge strategy, plus the same retry loop |

Recommendation: **plain refs.** Master-master, many writers, no shared mutable tip: the same
reason the signed-evidence work uses one ref per record. The branch-of-files option is the
fallback if someone insists on browsing the queue in GitHub's UI. A notes ref per bundle
collapses back into plain refs.

**Freshness without the GitHub API.** A machine mirrors the remote's `refs/queue/*` into
`refs/queue-seen/<remote>/*` with one `git fetch --prune`. Poll as often as you like
(`ls-remote` is cheaper; fetch only when it changed). A publisher may also send a message
as a nudge. **Correctness never depends on freshness.** A stale list can only cost a wasted
attempt, because every decision is re-checked at the point where it is made (section 3).

## 3. How a queue pulls

1. **Look.** Fetch refs from the target remote (the one that holds `main`) and any other
   sources. Resolve each bundle and **check the content**: the tip's tree hash must equal the
   `<tree>` in the ref's name. If it does not, the bundle is not what was advertised (someone
   moved the ref), and it is refused. If the tip commit cannot be fetched, the bundle is
   "gone". If two publishers offer the same tree, any valid one will do.
2. **Order.** By the tip commit's date, then tree hash. Every machine computes the same order
   from what it can see; no coordination.
3. **Claim.** Create `refs/queue/claim/<tree>` on the target with an *expect-absent* push.
   Git makes ref creation atomic, so of any number of machines racing, exactly one succeeds.
   The claim is a small blob: who, a random nonce, when, and an expiry (default 5 minutes).
4. **Look again** after claiming (the world may have moved): already landed, or content
   changed, means release and stop.
5. **Land.** If `main` is an ancestor of the tip, fast-forward to the exact advertised
   commit. Otherwise replay the bundle's commits on top of `main` (a work-tree-free
   cherry-pick; conflicts are refused). Optionally run a check command on an export of the
   result. Then **one atomic push** does three things together: move `main` (only if it is
   still where we saw it), delete our claim (only if it is still ours), and create
   `refs/queue/landed/<tree>`. If `main` moved, rebuild and retry. If our claim was taken
   over, the push is refused, so a machine that stalled cannot land late.
6. **Disappearing.** If a machine dies holding a claim, the claim expires. Another machine
   takes over with a compare-and-swap on that exact claim value. Expiry uses the claimer's
   clock, so keep the lifetime well above clock skew.

No locks: the only arbiter is a compare-and-swap on a ref, which git already gives `main`.
No global view: each machine acts on its own fetch and the atomic push settles disputes.
Machines run alone, together, or in different places. The queue pulls from any number of
sources, but claims and `main` live on one target, because `main` is already a single place.

## 4. Signed evidence

Evidence is already keyed by tree, in `refs/notes/evidence/<record>` (see `gate-evidence.md`).
A bundle names its tree, so a trusted note over that tree is the bundle's evidence. The
publisher pushes the note refs next to the bundle, and a queue fetches
`refs/notes/evidence/*` from the same sources. After claiming, the queue asks, "does a
trusted note cover the tree I am about to land?" If yes, it skips the check. One subtlety,
tested: coverage holds only when the landing tree *is* the bundle tree, which means a
fast-forward. After `main` moves and the bundle is replayed, the tree is new and needs a
fresh run. The prototype takes the "trusted?" question as a command (`--evidence-cmd`); it
does not call `st evidence verify` yet.

## 5. Lost and gained against GitHub's merge queue

Lost:

- GitHub's integration: no PR status, no required-check enforcement by GitHub, no queue page.
  The queue pushes to `main` directly, so rulesets that demand a PR would have to let the
  queue's identity bypass them. That is a rollout decision I did not touch.
- Speculative batches: GitHub tests several queued PRs stacked on each other in parallel. The
  prototype lands one at a time, in order.
- Review and comment flow stays with PRs. A bundle has no thread.
- Expiry trusts clocks. Anyone who can push to `refs/queue/` can offer or claim, so access
  to the target is the access control; evidence is what makes a bundle trustworthy.
- Ref clutter on the remote, and a slow-to-notice stuck queue (nothing shows it but `gq ls`).

Gained:

- Pulls from anywhere git can fetch: GitHub, Nathan's machine, Johannes's machine. If GitHub
  is down, machines can still queue from each other.
- No API rate limits, webhooks, or polling the GitHub API.
- Identity by content: rebasing or reformatting just makes a new bundle, and there are no
  change ids to keep in step.
- Many queue machines, no election and no lock service; a dead machine's work is picked up.
- Cheap to read, test, and replace: it is a script over git.

## What the prototype does and does not do

Does: publish both forms, list, claim, land (fast-forward or replay), run loop, pull from
several sources, optional check and evidence skip. Tests: 19, including eight machines
racing one claim (one winner), two clones racing one landing, stale list, tree changed after
advertising (both forms), a dead holder's claim expiring and a late machine refused.

Does not: merge commits or root commits in a bundle, speculative batches, call
`st evidence verify`, prune old `landed` tombstones, notice a stuck queue, or touch GitHub
in any way.

## GitHub result (throwaway private repository, 2026-10-14)

Tested on `myobie/git-queue-scratch-20261014` (private, no rulesets, default settings):

| Try | Result |
| --- | --- |
| push a ref to a **blob** under `refs/queue/ready/...` | accepted |
| push a ref to a **tree** under `refs/queue/tree/...` | accepted |
| push a ref to a commit under `refs/queue/keep/...` | accepted |
| push a blob to `refs/heads/...` | **refused** ("remote rejected") |
| claim: create with expect-absent (`--force-with-lease=REF:`) | accepted when absent; refused when present |
| 6 clones push a claim for the same name at once | exactly **one** succeeded, five refused |
| atomic push: move `main` + delete a claim (leased on its value) + create a tombstone | accepted as one unit |
| `ls-remote origin 'refs/queue/*'`; fresh `fetch` of `refs/queue/*` | returns blob, tree and commit refs; blobs and trees fetch fine |

Cheap numbers (one run from this machine, so only indicative): with 1,005 queue refs on the
remote, `ls-remote 'refs/queue/*'` took 0.86 s (the same as listing one ref, 0.85 s, so it is
all round trip), a fresh `fetch` of all of them 1.2 s, and one push creating 1,000 blob refs
3.6 s. The prototype's `gq` also ran end to end against it: a two-machine race over GitHub
had one winner, and a range-form (blob) bundle landed through the atomic push.

Not tested: refs under rulesets, very large ref counts (tens of thousands), GitHub's
behaviour when many pushes arrive in the same millisecond beyond the 6-way race above, and
whether GitHub eventually garbage-collects objects held only by a blob ref (it holds them
while the ref exists). The repository could not be deleted with the available token (it
lacks `delete_repo`), so it still exists, private and empty of anything but test refs.

Found along the way: a `landed` marker with unexpected contents was not counted as landed
(fixed, with a test). Nothing was wrong with git or GitHub.

## Who would push to `main` in a real rollout

Not decided here; nothing was changed. If the queue lands directly, the pusher must be an
identity that can write `main`. Today `main` accepts changes only through the GitHub merge
queue, so a real rollout would need one of: a dedicated machine user or GitHub App for the
queue with a **ruleset bypass** for it (the simplest, and the setting Nathan has to approve),
or leaving the queue's result as a PR it opens and the GitHub queue merges. Without such a
setting the push to `main` is refused, and the atomic landing push fails whole. That
rejection is safe: nothing else in it is applied.

## Questions for Nathan

1. Which identity pushes to `main`, and is a ruleset bypass for it acceptable? (Not touched.)
2. `refs/queue/` is fine for the prototype (confirmed).

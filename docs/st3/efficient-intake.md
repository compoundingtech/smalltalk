# Efficient repository intake

Status: design for the five stopped repository intakes. Their owner turns each intake back on after implementation and review. Publishing a revised mission by itself does not restart a cancelled intake.

## The unit of work

Review one **ready pull request head**, identified by `(GitHub repository ID, pull request number, head SHA)`. Triage one issue, identified by `(GitHub repository ID, issue number)`. Repository ID survives a rename. A new head may need a new review; title edits, closure, reopening at the same head, a daemon restart, and an intake or seat restart do not. Issues are triaged once unless a person explicitly requests another triage. Drafts enter intake when first ready. The GitHub issues endpoint must continue to exclude pull requests.

The repository observer keeps the complete paginated listing and normalized item resources. The first complete observation is a baseline: it records existing items without requesting reviews or triage. It must not treat a page formerly missed by a partial reader as newly opened. A failed or partial listing leaves the previous complete facts intact. On each later observation, compare the PR's head and readiness and the issue's number, then consider only changed delivery keys. The current implementation already keeps repository IDs, retains items, filters drafts, and pins item resource claims, but its PR discovery also fires when `state` or `draft` changes. Intake should reject those changes when the head has already been delivered.

## Durable delivery record

Record a stable delivery key on a durable graph subject independent of the intake mission run, subscription subject, seat, and review mission revision. The key includes the repository ID, item number, kind (`review` or `triage`), and PR head SHA where applicable. Atomically create its request record and pin the item resource observation claim used as the run input. A second observation or a replacement subscription finds that record and cannot create another run for the key. Reconciliation retries that same request and run with the existing idempotency key. The run's completion links its review document or triage result to the key. A request in progress prevents duplicate work; it does **not** claim the item was reviewed. A failed run remains failed and visible for an explicit retry of the same key, without automatic re-delivery as a new run.

Keep the record when an intake run or subscription is retired. Cancellation stops unstarted requests from that subscription, but it does not erase delivery history. When an intake is turned on again, it resumes from these records and the repository resource baseline. Migration must index already started and completed subscription runs by their pinned item observation claims before enabling the new intakes. For an old open item with no provable claim or prior run, establish a baseline and require a new head or an explicit person release; do not guess that it needs another review. This prevents the 30-open-PR replay seen after the pty-rust seat restart.

The key and request are graph facts, not a local cursor. The provider's ETag, next poll deadline, and process memory are only fetch optimizations. An intake, seat, or daemon restart can make another GitHub request, but it cannot clear delivery history. The host that declared the subscription remains the only run starter; an idempotent start and `subscription.mission-started` receipt resolve a crash between run creation and receipt.

## Pull requests already reviewed by their authoring mission

An authoring mission that has its own reviewer or human review gate stamps the PR body at creation with its exact mission-run subject and review resource or gate subject. It also publishes the `vcs.pull-request` resource with repository ID, number, URL, and head SHA from that same run. The repository provider reads the marker with the PR listing. Intake verifies that the named run owns the matching PR resource and has an internal review step or gate. It then records `skipped: authoring mission owns review` for the delivery key, citing that graph evidence, without starting a second reviewer. A GitHub author name or branch prefix alone does not establish ownership: several agents may use one account, and branch names can be reused.

If the marker names a run whose resource has not replicated yet, leave the delivery key pending and retry the graph lookup. An invalid marker never authorizes a skip; surface the mismatch for correction. PRs without the marker enter ordinary intake. This convention must be added to the authoring missions before their repository intake is enabled, so a PR cannot race ahead of its provenance. For existing PRs, use only a verified graph PR resource and review owner; otherwise keep them in the baseline and do not replay them automatically.

## Mission revision and load

Allow `delivery "mission"` to name a ready mission by stable name, without `@revision`. Resolve the current ready revision **when the request starts** and write that exact revision into the run creation claim. A capacity-blocked request therefore picks up a review mission published while it waited. Already running reviews retain their exact revision; use normal run revision if they need new instructions. Keep explicit `mission@revision` for callers that require a fixed revision. Changing or restarting the intake never resets delivery keys. This is a small extension to the existing subscription request and start path; it avoids a second scheduler.

Set `concurrent-runs max=2` on each review and triage mission, with a conservative host-wide build budget. The current subscription cap of five requests per observation is a separate burst guard; requests beyond it remain held for a person. Capacity leaves requests pending and retries them with backoff. Limit worker turns and retries so an invalid claim cannot cycle to `max-rounds` or flood the owner's attention. Validate review claims against the exact input claim and head SHA before completing a review. A reviewer sends findings to the authoring agent when appropriate; it raises person attention only for an explicit decision that the authoring mission cannot make.

## Review workspace

Create one review directory per delivery key under a disk-backed state path such as `/srv/st3/runs/<repo>/reviews/<run-id>`. Never clone or build in `/tmp` or another tmpfs. Fetch the pinned head into that directory, verify the checked-out SHA, and merge `origin/main` before tests; PRs merge on `st/ci`. Use the host's shared sccache through the configured `RUSTC_WRAPPER`, with bounded Cargo jobs and a per-run target directory or an explicitly serialized shared target. Cache compiled dependencies across runs without sharing a mutable worktree. The run owns the directory. Terminal cleanup stops the reviewer, removes only that run directory and its target artifacts, and records cleanup success; an orphan sweeper removes directories whose graph runs are terminal after a crash. Preserve the immutable review document and graph claims, not gigabytes of checkout and build output.

## Proof before re-enabling

Use isolated daemons to prove: one new head requests one run; the same head survives observer, intake, seat, and daemon restarts without a second run; a new head requests one more; an issue receives one triage; draft-to-ready requests once; title and state changes do not; a verified authoring mission PR is skipped; an unverified marker cannot skip; capacity and crash retries reuse the request; a newly published review mission revision is selected for a pending or future request. Check the migration against the old open-PR set without launching reviews. Verify the build path is disk-backed, sccache is shared, and terminal cleanup reclaims a failed run's directory. Keep all five existing intakes cancelled during this proof.

After the implementation, an operator prepares one repository's intake KDL with its repository locator, stable resource identity, review and triage mission names, disk-backed workspace, and requester. The owner's final turn-on is only:

```sh
"$ST3_BIN" missions publish missions/REPO-intake.kdl --as person/owner
"$ST3_BIN" missions start fleet/REPO/intake --workspace /srv/st3/runs/REPO --as person/owner
```

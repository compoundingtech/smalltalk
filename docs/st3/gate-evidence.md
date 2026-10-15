# Gate evidence

`st evidence put`, `show`, and `verify` run offline. They require Git with SSH
signing support, OpenSSH, repository objects, and an operator-supplied allowed
signers file. They do not load daemon configuration or contact an st instance.

One immutable record represents one observation; its trust tier is explicit. The first kinds are `tests` and
`review`. `produced-by` is reserved as a name for a future trusted observer's
verified trace receipt; v1 rejects it until its predicate and verifier exist.

## Record and transport

The record is an in-toto v1 statement with one subject: `name` is the observed
full commit ID, and `digest.gitTree` is its exact Git tree ID. The predicate type
is `https://smalltalk.dev/evidence/gate/v1`. Its fields are:

| Field | Meaning |
| --- | --- |
| `schema_version` | Predicate schema version, initially `1`; unknown versions fail closed. |
| `change_id` | Nonempty free label in the signed predicate; supplied by the workflow or the wrapper. Nothing matches or filters on it. |
| `signer` | Exact SSH principal in the caller's allowed-signers file, or null for degraded records. |
| `tier` | `full`, `degraded`, or `test`; a signature alone never promotes a tier. |
| `re_attests` | Optional degraded record ID when a fresh full-trust test run re-attests the same keyed check. |
| `producer_sha256` | Lowercase SHA-256 of the signing observer's running executable, measured inside the signing API. |
| `observed_at` | RFC 3339 time recorded by the trusted observer. |
| `observed` | A typed `tests` or `review` observation. |

A `tests` observation names its `runner` and maps check names to `command`
(argument vector), `exit_code`, and `output_sha256` (exact captured output
bytes). Its open `inputs` map carries opaque target/input keys and digest strings
for future affected-set observers, and defaults to empty. v1 preserves those keys
but still requires the whole exact tree; it does not infer scoped equivalence. The observer must construct these from its run, including failed checks.

A `review` observation names its `reviewer`, `verdict` (`GO` or `STOP`), and
`base_commit`, an ancestor of the observed commit. Its `files` map covers the complete changed-path set from base to
observed commit, with each path's Git blob ID or null for deletion. Renames
appear as deletion plus addition; submodules use their commit ID. `tests` lists
immutable signed test record IDs the reviewer considered. The observer must
obtain the verdict from its review execution and derive file hashes itself.

The statement is the sole regular-file blob in a notes tree, named by its
subject's full tree ID. For full/test records, `git commit-tree -S` signs that notes tree using Git's
SSH signing. Its only parent is the observed source commit, so transporting the
note also retains the source tree and review base history. The signature covers
the exact JSON bytes and attachment, without inventing a JSON canonicalization
or a detached signature scheme. This is in-toto statement JSON inside a signed
Git commit, **not** a DSSE envelope or a GitHub artifact attestation.

The immutable record ID is this notes commit's full object ID. Its only ref is
`refs/notes/evidence/<record-id>`. There is no shared mutable notes tip: different
writers create different refs, so their updates cannot conflict. Duplicate
imports verify the signature and create the ref transactionally, or confirm it
already points to the same ID. A ref whose suffix does not equal its target is
invalid. The flat notes tree remains readable with Git's notes command:

```sh
git notes --ref=refs/notes/evidence/RECORD_ID show TREE_ID
git push origin 'refs/notes/evidence/*:refs/notes/evidence/*'
git fetch origin 'refs/notes/evidence/*:refs/notes/evidence/*'
```

Keep the namespace when synchronizing repositories; do not prune records as a
side effect of branch cleanup. The wildcard refspec is additive and does not
need force updates or a merge of notes histories. v1 uses one ref per record;
large-repository indexing and retention belong to later stages.

## Trust and custody

The CLI never takes a JSON payload or a signing key. `put` imports an
already-constructed observation: full/test must be signed; degraded must be
explicitly unsigned with a null signer. Fetch its objects before importing:

```sh
st evidence put --repo . --record RECORD_ID --trusted-signers /tmp/allowed-signers
st evidence show --repo . --record RECORD_ID
st evidence verify --repo . --record RECORD_ID --trusted-signers /tmp/allowed-signers
```

`show` prints data and asserts no trust. `verify --record` checks the format,
attachment, source tree, signature, and exact signer principal for full/test.
For degraded it validates the format and confirms the absence of a signature.
CLI results report the tier and `signature_verified` explicitly. The trust file
uses OpenSSH allowed-signers syntax, for example a distinct public key for each
principal, restricted to `namespaces="git"`. Keep it outside the untrusted
checkout. A missing key, forged signature, mismatched signer, unsupported kind,
or malformed statement supplies no evidence. Repository Git signing programs,
allowed-signers paths, revocation files, and replacement objects cannot widen
this caller-supplied trust. Key removal from the trust list revokes its evidence.

`st3::evidence::sign_observation` is an API for trusted observers running under
a separate signing account. It validates the statement, measures its own
executable digest, constructs the note, and calls Git's SSH signer. The observer
must construct facts from its own execution; it must never expose this API as a
service that signs arbitrary agent-authored JSON. Test keys are generated in
temporary directories. No production key or key path is shipped here.

The protected CI observer described below writes records from an operator-owned
sekrets profile. An intake review observation adapter is not implemented. Custody
uses a profile whose only granted command is the installed, pinned observer. Custody and trace-receipt isolation
share an OS-account boundary; gate evidence does not claim native agent identity
or reimplement that separate receipt protocol.

## Gate policy and reuse

`.st/evidence-policy.json` in the accepted base commit declares the initial
`daemon` risk class, required `linux-gate` check, allowed `full` tier, and CI/review principal roles.
The workflow must supply an accepted, immutable base: the proposed target cannot
weaken its own required checks or signer roles. A policy change is judged under
the old accepted policy. Missing base policy fails closed.
These generic principals are provisioning targets; no key is trusted merely
because repository policy names it. The caller must separately supply its
operator-controlled trust list. Other risk classes require a later policy
version. The file affects this verifier only; it does not activate
curator enforcement or alter the merge queue.

```sh
st evidence verify --repo . --revision HEAD --base BASE_COMMIT \
  --change-id CHANGE_ID --trusted-signers /tmp/allowed-signers
```

Verification selects the newest trusted review for this change ID whose entire
changed-path set and hashes match the current base/target diff. Observation
time orders reviews; a STOP wins equal-time ties, then record IDs order ties.
A newer applicable STOP or incomplete GO cannot be hidden by an older GO. The
selected GO must cite imported, trusted `tests` records for this exact target
tree and change ID, from permitted test signers. Every cited check must pass,
and their union must contain every required check. Its successful result reports `tier: full`. Otherwise the command exits
nonzero and blocks the stage. Test tier never satisfies production policy.
The initial daemon policy also rejects degraded records, even with passing
checks; there is no low-risk exception. Invalid or untrusted individual records cannot
satisfy a gate, and are ignored when selecting usable evidence; malformed or
rewritten namespace refs fail the transport integrity check.

The CI observer uses `record_degraded` when the protected signer is unavailable.
CI runs independently of the post-completion observer. It preserves the
measured facts in a separate explicitly degraded record; production verification
remains fail-closed under the selected policy. Low-risk policy allowances and
re-attestation deadlines require the gate step and its policy decision. A fresh
full-trust rerun may cite the original with `re_attests`; the verifier checks its
subject, change ID, check names/commands, opaque input keys, and time against that degraded record.
It never changes the original tier. Observing the same old bytes without running
the check again cannot qualify as a re-attestation.

The CI observer reports full-trust record share, signing latency and linked fresh
re-attestation timing as described below. Its throwaway-key test mode emits only
test tier and expires within 15 minutes. Test tier never satisfies production policy.

A rebase that preserves the reviewed paths, their blobs, and the cited test
records keeps the GO. A new unreviewed path, changed blob, deletion, different
change ID, missing citation, failed check, wrong signer role, or changed tested
tree blocks reuse. v1 tests cover the entire exact tree: a rebase incorporating
unrelated source changes needs new tests and a GO citing those tests. Scoped
test input equivalence belongs to affected-set evidence and must not be inferred
from unchanged review blobs alone. Executable modes are covered by the exact
tested tree even though the review map records blob identities.

Observation scheduling, evidence publication, intake signing and typed review-gate
waits require separate integration. The merge queue remains the landing path.

## CI source receipt

Workspace CI retains a source receipt in the non-required `evidence-source` job.
It runs alongside existing checks, has only read permission, and holds no signing
key. Failure to produce or upload the receipt leaves the required checks running.
The actual failed step remains visible; no check result is inferred from a receipt.

For a pull request, GitHub's workflow and check-run `head_sha` identify the author
head. CI's checkout can instead be the synthetic merge `GITHUB_SHA`. The receipt
records `git rev-parse HEAD` and its tree and requires that HEAD equals the
effective `GITHUB_SHA`. Later evidence observers must use this source rather than
guess the tested tree from run metadata.

The artifact `ci-evidence-source-RUN_ATTEMPT` contains exactly:

- `receipt.json`: schema version, repository, event, run ID, attempt, complete
  source and tree IDs, object format, change placeholder, producer script SHA-256,
  and the pack's byte length and SHA-256.
- `source.pack`: the observed commit and its complete tree and blob objects.
  It carries no refs, repository configuration, executable observer or signing key.
  Commit history is excluded, so a depth-one checkout suffices. Gitlinks retain
  their commit IDs without copying submodule repositories.

The workflow embeds its producer in a temporary file outside the checkout. The
receipt measures those actual script bytes. The artifact is retained for 14 days,
including source objects whose synthetic merge ref has since moved or disappeared.

`change_id` is a free label in the signed predicate. Today it is `github:OWNER/REPO#NUMBER`
for a PR or merge queue entry in the source receipt, `github:OWNER/REPO@SHA` in the aggregate
note, and `local:<commit>` from the local wrapper. Nothing reads a commit message, and no
trailer or ref is needed. A queue entry can contain additional changes: its source
and tree still identify the whole tested checkout. A manual run uses
`github:OWNER/REPO@SOURCE`, without inventing a PR identity.

This receipt supplies source provenance for the record adapter; it is not a signed
evidence record and authorizes no gate. The protected observer must independently
authenticate artifact size/digest and repository/run/attempt identity, validate
the producer against its pinned recipe, and derive the tree from the imported Git
objects. It must observe completed check results separately and refuse mismatches.
Our cooperative v1 runner boundary does not prove process identity against a
hostile workflow author; native observer identity is a separate prerequisite.

Run `python3 scripts/ci-evidence-source-test` to exercise real Git merges, shallow
checkouts, object import, replacement-object resistance and provenance mismatches.

## Protected CI observer

The internal `st3 evidence observe-ci --run RUN --attempt ATTEMPT --artifact ID`
command accepts numeric selectors only. It reads its operator-owned configuration
from the profile's private HOME at `.config/st/evidence-ci.json`. A sekrets grant
must allow only that installed command prefix, after review and binary pinning.
The caller cannot supply a statement, repository, check result, signer or key.

The private configuration names the repository, absolute private bare
`scratch_repo`, operator-selected immutable `accepted_base`, pinned
`recipe_sha256` for `scripts/ci-evidence-source`, pinned `workflow_sha256` for
`.github/workflows/fleet.yml`, and `required_checks`. It sets `tier` to `full`, `degraded` or `test`. Full mode also
names `signer` and the absolute private `ssh_key`; degraded mode has neither.
Test mode requires a throwaway key and `expires_at`, no more than 15 minutes
ahead. Use a separate scratch repository for the test service. Production key
files and configuration are never stored in a change checkout or this repository.

The protected PATH supplies operator-installed st3, Git, OpenSSH, gh, Python and
GNU timeout. gh receives only the profile's read-only GitHub credential. The
observer fetches attempt-specific run metadata and every page of jobs; it checks
the configured repository, Workspace CI workflow, completed state and required
check names. Missing, duplicate or incomplete jobs are refused. It checks the
source producer's actual steps, including upload, even when the optional job's
overall result is softened by `continue-on-error`.

Artifact identity, byte length and SHA-256 come from authenticated GitHub
metadata. The complete ZIP must match before extraction. Exactly two regular
files are allowed, with bounded sizes: receipt JSON and the Git object pack.
The receipt must bind the run/attempt and pinned recipe, and its pack digest
must match. The observer fetches the accepted baseline and authenticated platform
head (plus fixed main history for PR merge parents) from its configured public
repository, with fsck enabled, then imports the bounded source pack with
`index-pack --strict`. Git independently derives the source tree and producer
script digest; neither the artifact nor its self-reported producer hash proves
what executed.

Full trust is limited to `merge_group` and `workflow_dispatch`: their retained
source must equal the platform's authenticated `run.head_sha`. Both the workflow
and producer recipe must exactly match the operator-selected accepted base and
its pins. A workflow change is refused for full trust until separately reviewed
and accepted by the operator. `pull_request` cannot prove the effective tested
merge: its author controls the artifact while run metadata names the author head.
Production PR observations are always degraded, even with a pinned recipe; test
services may emit test tier. A PR observation never satisfies production policy.
The response explicitly reports full eligibility and any trust limitation. The
core producer API refuses a request to sign ineligible source at full tier.

Each required check gets one record, including failures. The command descriptor
identifies the GitHub workflow/check, while `runner` retains run, attempt and job
provenance. `output_sha256` hashes the serialized observed job JSON, rather than
claiming the observer ran cargo or captured its stdout. Skipped, neutral, cancelled
and failed jobs all have a failing exit code.

When signing fails, the same measured facts become explicitly unsigned degraded
records. A local per-job reference retains that result: retrying the same job
after the key returns does not promote it. A fresh keyed rerun is required.
The full record and observation reference are created in one Git transaction.
Test services emit test tier; production policy rejects those records even when
their signatures verify. An expired test service refuses to produce records.

The response contains record IDs, explicit tiers, and an object-only base64 pack
with source and evidence objects. The caller imports it with `git index-pack --stdin`, then uses `st evidence put` with its external allowed-signers file.
No key, configuration, ref or executable is exported. The caller retains/fetches
source parent history before pushing evidence refs to peers.

Batch metrics count newly recorded jobs only: full records / fresh records,
full-trust share (null with a zero denominator), and signing wall time. Original
CI receives zero signing wait because this adapter runs after check completion.
Re-attestation elapsed time remains null until a separately observed rerun is
explicitly linked; it is not reported as zero. The controller that schedules
observations and outage fallback is a separate integration.

The cooperative v1 receipt boundary trusts our runner agents. Pinning recipes
and isolating the key does not prove native process-to-agent identity against a
hostile workflow author. These limits are why full tier requires a platform-bound
queue/manual source and an accepted workflow pin. Native receipts remain separate
work; this does not certify the behavior of all target code or runtime identity.

## Evidence observe workflow

`.github/workflows/evidence-observe.yml` starts from a finished merge-group run of
`Workspace CI` (or a manual replay from `main`) on runners labelled `evidence-ci`. It never runs
for a pull request or a fork. `scripts/ci-evidence-observe` asks the protected observer
(`$EVIDENCE_OBSERVER`, the operator's JSON argument vector; default
`st sekrets -- st3 evidence observe-ci`) to record the run, imports the returned pack into a
persistent scratch repository, verifies every full-trust record with `scripts/evidence-verify`
against the key list in `main`, creates `refs/notes/evidence/<record>` and pushes it. It runs no
check, never replaces an existing ref, and publishes degraded records labelled as unsigned and
unverified. A full record that does not verify fails the job and creates no ref. A cancelled
run has no `linux-gate` job to record (the observer exits 2): the workflow skips cancelled
runs, and a replay of one reports "nothing to attest" instead of failing. The observer's answer
is about 20 MB (the base64 object pack), so the script streams it to a file. The same code
serves a degraded profile (no key) and a full one; the operator's profile decides.

The same script runs outside GitHub Actions, for a timer on an operator host:

```
python3 scripts/ci-evidence-observe --poll --publish --checkout CHECKOUT_OF_MAIN \
  --keys-from origin/main --state-dir /var/lib/evidence-observe
python3 scripts/ci-evidence-observe --run RUN_ID --publish --checkout CHECKOUT_OF_MAIN   # one run
```

`--poll` lists the last `--limit` (20) completed merge-group `Workspace CI` runs, skips cancelled
or skipped ones and runs already done, observes the rest and records each in
`STATE_DIR/observed.jsonl`; a run that fails three times is marked `gave_up` and never retried
(loop protection). `--run` takes the run id alone (the attempt is read from GitHub). Environment:
`EVIDENCE_OBSERVER` (observer argument vector as JSON), `GH_TOKEN` or the operator's own `gh` login
for GitHub reads and the push, and the observer's own `HOME` for its private configuration. State
(`scratch.git`, `observed.jsonl`) lives in `--state-dir` (default `~/.local/state/evidence-observe`).

The controller never records a check that the evidence gate skipped. A gate job that decides to skip ends
with a step named `evidence skip taken` and still concludes success; if the controller recorded the check
anyway, one real run would keep refreshing its own skips. It therefore reads the run's jobs, and any record
whose check has an `evidence-gate-<check>` job whose `evidence skip taken` step ran, or whose own job ran the
step `evidence covered by a trusted note` (a second, independent signal), is neither imported nor
published (the summary names it). If the jobs cannot be read nothing is recorded. It also refuses any run
that is not a run of `Workspace CI`, so it can never act on its own workflow's runs.

The server rejects a shallow push, and the pack carries the source commit without its history, so
the script first fetches the source's missing parents from the repository. Run
`python3 scripts/ci-evidence-observe-test` for the cases (signature verified, verifier refusal,
wrong run, malformed pack, push with a missing parent, replay).

## Recording a local run: evidence-wrap and evidence-sign

`scripts/evidence-wrap --check NAME -- COMMAND...` runs one check once, from a clean checkout, on
the developer's machine, and asks the signer to record it. It measures the commit, the command,
its exit code, the sha256 of its exact output, and an environment fingerprint: the sha256 of
`flake.lock`, of the dev shell derivation path, of the wrapper's own bytes, the system and the
attempt. A failing command is recorded too. It refuses a dirty checkout and a run that changes the
tree. Take the script from `main`, not from the change under test.

`scripts/evidence-sign` runs where the key is (root-installed, hash-pinned, behind a sekrets grant
that allows only that command; configuration in `$HOME/.config/st/evidence-sign.json`). It reads
the commit, tree and `flake.lock` from the caller's checkout, never from the request, uses its own
clock and its own digest as producer, accepts exactly the documented request keys, and signs a
`tests` observation with `runner: "wrapper"`. It cannot check that the command really ran; that is
the cooperative boundary (agents with the key's account can lie) and belongs in the known defects.

`scripts/testdata/evidence/wrapper-note/` is a golden note made with a throwaway key
(`python3 scripts/evidence-wrap-test --write-fixture DIR` regenerates one). The verifier's tests
read it too. A note applies only to the tree it names, so it helps CI only when the pushed commit
has the tree CI will test.

## A CI job that signs its own result

The machine that ran a check can sign the result itself, with no call to GitHub. The key is the
`EVIDENCE_SIGNING_KEY` secret of the `evidence-signing` environment (forks never receive it).
`scripts/ci-evidence-signing` holds the steps that handle it, and all of them are tested without
GitHub: `guard` allows only merge-queue refs, `main`, manual runs on `main` and same-repository
pull requests whose opener, run trigger and commit authors are on the list `.st/evidence-authors.json`
at the base commit (GitHub logins are authenticated; commit author emails are free text and only
catch a stray commit by someone else; a fork never signs); `configure` writes the signer configuration and key as mode 0600 in a 0700
directory and masks every key line; `once CHECK` refuses a second signature for a check in one job;
`cleanup` overwrites and removes the key, and runs even when earlier steps failed. The wrapper and
signer scripts must come from the base commit of the change, never from the tree under test.
Accepted gap: code under test in a job that holds the key can read it. That now includes every pull
request from this repository: its author controls the workflow file the run executes, so a person or agent with
write access can read the key and sign any note; the author list narrows who gets a note, it does not
stop that person's code from reading the key. Forks never get it. Narrowing the `evidence-signing`
environment back to the merge queue and main undoes this.

An aggregate job records what it knows: `scripts/ci-evidence-signing aggregate FILE` builds, from
the `needs` context, the needed jobs, the result each reported and the gate job's own result, and
`scripts/evidence-wrap --aggregate FILE` signs that. It does not claim the stages' commands,
outputs, test inventories, timings or toolchains, because it did not run them; those belong in
notes signed by the stage jobs. The signer accepts extra named string inputs next to the required
environment fingerprint, so a check's note can carry more over time; the repository policy decides
which inputs a check must carry.

### Stage notes and publishing

Each `linux-gate` stage job (the test shards, clippy, fleet-compat and the mail canaries) signs its
own result in fail-soft steps after its logs are retained, so signing cannot change the stage's
outcome. `ci-evidence-signing stage-facts` reads what `scripts/ci-linux` wrote: the exit status and
seconds (`STAGE.time`) and the digest of the exact log (`STAGE.log`); a missing or unreadable timing
file is recorded as a failure, never as success. `evidence-wrap --aggregate FILE --out DIR` signs
those facts (and records `workspace_clean` rather than refusing a workspace the tests dirtied) and
writes the note as a pack for upload. A stage job's checkout is shallow and the server refuses a push
from one, so the aggregate note job collects the packs and `scripts/evidence-publish` publishes them:
it accepts a note only if its signature verifies against `.st/evidence-keys` from the base commit for
the principal it claims, it is a full-tier single-note commit about this commit and tree, its check
is one of the allowed stage names, and the commit is its only parent. It never replaces a ref and
accepts at most 40 notes in one run; failures and refusals appear in the job summary by record id.

### What reaches the signing environment

Only the merge queue and `main` reach the `evidence-signing` environment: every signing job's
`environment:` is the expression `merge_group` or push to `main` ? `evidence-signing` : empty, and the
aggregate note job does not run for pull requests at all. A pull request, including one from this
repository, runs the same jobs without the environment, gets no key, and the guard step refuses it again
in code. **The environment must not gain required reviewers or a wait timer:** eleven required check jobs
carry it in the merge queue, so a protection rule that waits would block every required check. A
deployment-branch policy comes only after this conditional is on main (a policy that excludes the queue
refs would otherwise fail those jobs at start). Signing jobs also refuse to sign when tracked files
differ from the commit (`git status --porcelain --untracked-files=no`): the code under test ran in that
checkout, and the note names `HEAD`'s tree. Untracked build output is recorded (`untracked_files`) and
does not stop the note. `git status` can be told to ignore a tracked file (`--assume-unchanged`,
`--skip-worktree`), so the index flags are read too (`git ls-files -v`: every tracked file must be tagged
`H`). Untracked and ignored files are not refused, by design, because build output lives there; the note
therefore says "tested" about a checkout whose tracked files equal the tree it names, plus whatever
untracked or ignored files the job had. The signer finds `git` and `ssh-keygen` only in its own path: the
standard system directories, or the `path` in its configuration (`EVIDENCE_TOOL_PATH` when
`ci-evidence-signing configure` writes it), which is how the Nix sandbox, where they live in
`/nix/store`, supplies them without trusting the caller's PATH.

## Fresh CI re-attestation

The protected CI adapter accepts repeatable `--re-attest RECORD_ID` selectors
alongside its run, attempt and artifact selectors. These are immutable degraded
check records, never caller statements. Configuration, repository, roles, keys,
workflow and recipe pins still come from the private operator profile.

The observer fetches predecessor refs from the fixed repository and verifies
that each contains one explicitly unsigned degraded CI check. It derives the
old repository/run/attempt/job identity from that record, then reads the original
attempt's run metadata and job objects from authenticated GitHub. The run must
belong to the configured repository and completed Workspace CI workflow path;
an unrelated workflow with the same check name cannot supply a predecessor.
It requires an exact job ID,
check name, conclusion, serialized job digest and completion timestamp match.
One run/job read pair per distinct old attempt supplies all its selected jobs.

The current run must independently pass the existing source and workflow trust
boundary. A link needs identical change identity, whole-tree digest, check name,
command and opaque input map. It also needs a different run or attempt and job,
with the fresh job starting after the old check completed. Reusing the original
job, changing an input, or inventing an old result cannot produce a link. There
is at most one predecessor per current required check. All links are validated
before any new observation is written.

Only a newly produced Full record gets `re_attests`. Signing failure preserves
the fresh facts at degraded tier with no completed re-attestation link. Replay
retains the original result and never promotes it. An already recorded Full job
cannot acquire a different link; request the link on its first observation or
run a fresh check. Test-tier services never produce Full re-attestations.

The data-only export includes linked predecessor notes and their source commits
so offline verification can validate the chain. The predecessor stays degraded;
its record is never modified. This is an explicit selector API, not automatic
rerun scheduling or shared fallback coordination. Production controllers still
need to publish refs and provide the correct predecessor selectors.

The response adds `re_attestations` samples and `re_attested_records`. Each sample
measures from the authenticated degraded check's completion to the signed Full
note's issuance time. This includes scheduling delay after the rerun completed;
using rerun completion alone would understate time to trusted evidence. Git note
issuance has one-second precision; elapsed values are expressed in milliseconds.
The batch's `re_attestation_elapsed_ms` is the maximum new sample, or null when
none exists. Replays contribute no new samples. Full-trust share and signing
latency keep their existing fresh-record denominators. Production aggregation,
risk-class deadlines and their enforcing policy remain separate work.

A predecessor's conclusion may be failed. A passing rerun still links to it: the
link says the rerun re-checks the same tree, not that the verdict is the same.
Consumers must read the new record's own result, never infer it from the link.
If the observer clock reads earlier than the degraded completion, the sample has
`elapsed_ms` null and `skewed` true; the signed record stays valid and the sample
is left out of `re_attestation_elapsed_ms`. That value is a measurement only, not
a policy input, until deadlines and the production controller are decided.

## Standalone verifier

`scripts/evidence-verify` answers one question without a built st: does a trusted,
signed note exist for this exact tree and this check? It needs only `git`,
`ssh-keygen` and Python 3, reads the same `refs/notes/evidence/<record-id>` notes
(one SSH-signed commit per record), and finishes in well under a second on a
repository with hundreds of records. `st evidence verify` stays the reference
implementation; the script's tests use real Git repositories and real SSH
signatures.

```sh
scripts/evidence-verify --repo . --revision HEAD --check typescript-client \
  --keys-from BASE_COMMIT [--command '["nix","build",".#checks.x86_64-linux.suite"]']
```

Exit 0 is satisfied, exit 1 is not satisfied, exit 2 is a usage error. Both print one JSON
document: `{"satisfied":true,"tree","check","record","kind","signer","trust_level","tier","observed_at","command"}`
or `{"satisfied":false,"reason","check","detail"}`.

Trust comes only from the key list: `--keys FILE`, or `--keys-from COMMIT`, which reads
`.st/evidence-keys` (OpenSSH allowed-signers syntax, `namespaces="git"`) from the
accepted base, never from the change under test. A signer's trust level is the part of
its principal before the first slash (`machine/ci1` is level `machine`); it is never
read from the record. Removing a line revokes every note that key signed.

`kind` comes from the record's `runner`: `nix`, `wrapper` (a command run once and signed by the wrapper) and `sekrets` name themselves, and a runner that starts with `github:` is kind
`observed`, the central observer's record of a finished GitHub job (`st evidence observe-ci`).
Anything else is `unsupported_kind`. An unsigned record at tier `degraded` is never evidence
and is reported as `degraded`, not as a signature fault, because it is what the observer writes
when its signer is unreachable. The latest
trusted record for the tree and check decides, so a newer failure supersedes an older
pass. Reasons: `no_record`, `stale_tree` (records exist only for other trees),
`wrong_check`, `bad_signature` (forged or unsigned), `untrusted_signer` (a valid
signature from a key not on the list, or a principal that differs from the record's
signer), `untrusted_tier`, `untrusted_level`, `wrong_runner`, `too_old`, `future_dated`, `degraded`, `unsupported_kind`, `check_failed`, `wrong_command`, `wrong_input` and
`error` (unreadable input: no decision was made, count it separately). Notes are found
by the tree they name with one `git cat-file --batch-check`, so the cost grows with the
number of evidence refs; pack refs, or add a per-tree index, before they reach the
thousands.

The notes may be fetched shallow, as a CI checkout does: the note commit's parent is read
from the raw commit, and when the subject commit is not in the local repository the signed
statement's tree stands. When the subject commit is present its tree must equal the
statement's, so a record cannot be rebound to a different tree. The tree claim is the
signer's: the signature covers the statement, so only a key on the list can make it.

`--kinds nix,wrapper` limits the record kinds a check accepts. `--max-age SECONDS` rejects a record observed longer ago than that (`too_old`) or claiming a time
more than two minutes ahead (`future_dated`). `--runner-prefix github:ORG/REPO/` requires the record
to come from this repository's runs (`wrong_runner`; keep the trailing slash, it is what stops `repo`
matching `repo2`). `--trust-levels fleet,machine` limits which signer trust levels a check accepts
(`untrusted_level`); a newer failure from a level the check does not accept still stops a skip.
A record claiming a time more than two minutes ahead is always `future_dated`, with or without
`--max-age`. Every one of these options defaults to permissive, so **a check that skips on an
observed record must pass `--kinds`, `--command`, `--runner-prefix`, `--trust-levels` and
`--max-age` explicitly** (or, better, `--policy-from`). Any unexpected exception is a JSON
refusal with reason `error`, never a traceback and never a pass.

`--bind-input KEY=PATH`
(repeatable) requires the record's input `KEY` to equal the sha256 of `PATH` as it is in
the tree being verified, for example `flake_lock_sha256=flake.lock`, so an environment
fingerprint is tied to the tree and not to a claim; any mismatch, missing key or missing
file is `wrong_input`. Record inputs are opaque nonempty strings; the producers use
`flake_lock_sha256`, `toolchain_sha256` and `wrapper_sha256` (digests) and `system` and
`attempt` (plain strings), and only keys named by `--bind-input` are interpreted. Success
and most failures report `attempts`, the number of trusted records for the tree and
check, so a producer can refuse to run a third time.

A check that calls the script must pass `--command` with the argument vector it
expects; without it, any command a trusted signer recorded under that check name
satisfies the check, and the recorded output hash is not compared. A record whose
`observed_at` has no time zone is not evidence, because naive and aware times cannot
be ordered against each other.

This slice verifies SSH-signed `tests` records. The raw Nix output-path signature
check for the typed `nix` observation is a later slice, agreed with the Nix producer.

## Evidence gate and the skip policy

Five non-required jobs, `evidence-gate-<check>` for each required check (`linux-gate`,
`typescript-client`, `isolation-vm`, `genie-freshness`, `mail-redelivery-canaries`), decide whether
a trusted note already covers the run's tree. They hold `contents: read` only, write nothing and dispatch
nothing, and every problem becomes `decision=run`. **Every required check now consumes its gate's decision**: `genie-freshness` and `typescript-client`
(by skipping their steps), and the Linux group (below).

### What a skip does

When `evidence-gate-<check>` decides `skip`, the job of that check still runs and still ends in success, so it
keeps its name, its place in the required set and every `needs` on it. It runs only the checkout and its own
cache-coverage report, skips every other step, and adds the step `evidence covered by a trusted note`, which
writes the record id to the job summary. The wiring is deliberately conservative:

- Each wired job also waits for its gate job (a hosted runner start, typically 10 to 30 seconds), so while no
  note exists these two checks start slightly later than before. That is the price of a single, auditable
  decision.
- A wired job's own `if` names a status function (`!cancelled()`), so a gate job that failed, was skipped or
  never started cannot skip the required check: its output is empty, which is not `'true'`, and the steps run.
- The checkout is never skipped, and a step that already had an `if` keeps it, combined with the skip
  condition. A step that does not know about the skip runs.
- The skip decision the check jobs read is the output of the `evidence skip taken` step itself, written as its last
  command, not of the earlier decision step. So `skip=true` cannot exist unless that marker step already ran: a gate
  job that dies between deciding and marking leaves no output and the check runs. The observer refuses to record a
  check whose gate job ran the marker (and should also refuse one whose own job ran `evidence covered by a trusted
  note`, the step a wired job runs only when it skipped its work), and the in-job producers refuse to sign for a
  stage that did not run, so a skip never produces a fresh note for itself.
- Every signing step is told when its gate skipped: it gets `GATE_SKIP` (true when any gate that can cover that job
  said skip), and the signing helper then makes no note whatever the job's log or step outcomes say. The aggregate
  note job `needs` every `evidence-gate-*` job, so it sees them all and refuses to sign when any says skip. Each
  gate a job reads is one of that job's own `needs`, which a test checks.
- A skip of `genie-freshness` skips the tests of this machinery too (`ci-evidence-gate-test`,
  `evidence-verify-test`, `genie --check`) for that tree. That is accepted because the note is for the same tree and
  a change to those files is a protected path that always runs in full; a reader must not assume they ran.
- The skip rests on a note of kind `observed`, which the central observer records only for a `merge_group`
  run, so a pull request can skip only a tree an earlier queue run already passed. Do not expect a saving at
  pull-request time from these two checks.
- Until a producer writes a note for a check, that check never skips. At the time of writing no note exists for
  these two, so both run in full; the first skip needs a producer.

### The Linux group

`linux-gate` is an aggregate of jobs that are not themselves required checks, so those jobs skip at the **job**
level, which allocates no runner at all: `linux-tests` and its three shards, `linux-clippy` and
`linux-fleet-compat` skip when `evidence-gate-linux-gate` decides skip. `linux-test-build` feeds the test shards,
`mail-redelivery-canaries` and `isolation-vm`, so it skips only when **every one of them** is covered; that
condition is derived from theirs in the generator, never written twice. The two jobs that are required checks in
their own right keep running and end in success with their steps skipped, so a required check never reports
`skipped`: `mail-redelivery-canaries` is covered by its own note or by `linux-gate`'s (the aggregate note proves
its canaries passed), `isolation-vm` by its own note; both gate their first step, the pre-check that the shared
build succeeded.

`linux-gate` itself still runs always and still fails on anything but success, with one addition: it reads its gate's
decision, and a member that reports `skipped` is accepted only when that decision was exactly `true`, only
in the **positions** of the stages that are skipped at job level (the build, the test shards, clippy and fleet
compatibility, derived in the generator from the very list that skips them), never for a failure or a
cancellation, and the stage counts are still enforced. `upgrade-impact` and `mail-redelivery-canaries` are never
accepted as skipped, so a condition that skipped them for any other reason cannot be waved through just because a
note exists. A skipped job reports `skipped` to
the aggregate evidence-note producers, which refuse to sign for a skipped member, so a skipped run writes no
fresh `linux-gate` note.

### The skip policy

`.st/evidence-skip-policy.json` is read from the accepted base (the pull request base or the merge
group base), never from the change under test, and says per check what a note must be:

```json
{"version": 1, "checks": {"linux-gate": {
  "trust_levels": ["fleet"],
  "enforce": false,
  "max_age_s": null,
  "kinds": {
    "observed": {"command": ["github-actions", "{repository}", ".github/workflows/fleet.yml", "{check}"],
                 "runner_prefix": "github:{repository}/"},
    "wrapper": {"command": ["..."], "required_inputs": ["flake_lock_sha256", "toolchain_sha256"],
                "bind_inputs": {"flake_lock_sha256": "flake.lock"}}}}}}
```

`enforce` is the per-check switch for skipping and is **off unless it says `true`** (absent means
false; any other value is an error). With it off the gate still looks the note up and verifies it, and
prints `outcome=satisfied` with `decision=run` and a reason starting `shadow:`, so the proof that a
note would have covered the check accumulates while the check keeps running as today. Turning a check
on is the one-line change `"enforce": false` to `"enforce": true` in this file, reviewed and queued
like any change (the file is read from the accepted base, so the change takes effect for the runs
after it merges).

Each kind has its own rule: the exact `command` the record must have run, an optional
`runner_prefix`, optional per-kind `trust_levels`, the `required_inputs` it must carry (every one a nonempty string) and
`bind_inputs`, which must equal the sha256 of a file as it is in the tree being verified. A kind's own
`trust_levels` replaces the check's for that kind only, so a weaker signer (say `local`, the CI's
own runner signer) can be admitted for `wrapper` notes without ever satisfying `observed`; a note
never changes kind, since the kind comes from the record's runner and each kind keeps its own
command and inputs.
**Adding an input to `required_inputs` tightens future notes without breaking anything**: a
note that lacks it simply stops satisfying the check (`missing_input`). `max_age_s` is `null`
by default (no age limit; a signature is meant to grow to cover more over time); a note claiming
a future time is always refused. The file is validated strictly: an unknown key is an error, so
a typo cannot silently weaken a rule, and a check with no entry never skips. The tree, the check
name, the command, the exit code and the output hash are part of every record and are always
checked. `observed` (the central observer's record of a finished job) and `wrapper` (a note the in-job producer signs
as `fleet/ci`) are listed for **every** required check, each with the exact command that producer records:
- `linux-gate`: `["linux-gate", "aggregate", <the aggregate's needed jobs, sorted>]`;
- `mail-redelivery-canaries`: the stage command `["bash", "scripts/ci-linux", "mail-redelivery-canaries"]`;
- `genie-freshness`: `["bash", "scripts/ci-genie-freshness"]`; `typescript-client`: `["bash", "scripts/ci-typescript-client"]`;
  `isolation-vm`: `["isolation-vm", "nixos-test-driver", "transport-isolation-vm", "sekrets-vm"]`.
Every wrapper rule requires `flake_lock_sha256`, `toolchain_sha256`, `wrapper_sha256`, `system` and `attempt`
and binds `flake_lock_sha256` to `flake.lock` in the verified tree; the stage rule also requires `stage` and
`job`, and the step-outcome rules `job`, `steps` and `step_outcomes`. No rule names a weaker signer level, so only
`fleet/ci` counts. A producer signs a step-outcome note only when every listed step actually ran, and refuses
when a gate in its run said skip, so a skipped run writes no note for itself. If a producer's note changes (a job
added to the aggregate, a step renamed), the policy must change with it; until it does, the notes simply stop
matching and the check runs.
Write `scripts/evidence-verify --policy-from BASE
--repository OWNER/NAME` to apply it by hand.

### Running the checks locally (`scripts/st-test`)

`scripts/st-test` runs each required check that has a local command (today `genie-freshness` and
`typescript-client`; `--list` prints them) once, through `evidence-wrap`, on the tree the pull request will be
tested as, and the host's signer records the exit code and output digest. GitHub tests the merge of the branch into
its base, so when the branch does not hold the base's tip the script computes the merge tree (`git merge-tree`) and
makes a merge commit with that tree; a merge that conflicts stops it. The command always runs in a fresh throwaway
worktree of that commit, so untracked or ignored files in your checkout cannot change the result. A failing run is
recorded too. `evidence-wrap`, `evidence-sign` and `evidence-verify` come from the accepted base. It prints one
line per check: result, seconds, and whether the verifier, from the base, now counts the tree as covered
(`untrusted_signer` until the host's key line is on main). `--push REMOTE` pushes the notes after they verify, with the identity git uses for REMOTE (or
`EVIDENCE_PUSH_TOKEN`); whether that identity may create `refs/notes/evidence/*` is a repository rule not yet confirmed for agent seats.
It runs `nix develop` and `pnpm install` from the tree under test, on the signing host as its user: code in that tree runs
there with the key within reach (cooperative trust, see Host keys). It changes no policy: a check skips only where `enforce` is true.

### When the gate says skip

`scripts/ci-evidence-gate` takes the verifier from the accepted base and asks it, with the policy,
about this run's tree. It prints `decision=skip` only if **all** of these hold, and otherwise
`decision=run`:

- the head repository is positively this repository (a fork or an unknown head never skips);
- the event is `pull_request` or `merge_group` (a manual run or a push never skips);
- the verifier exits 0 reporting `satisfied`, that it applied the policy, tier `full`, and a
  kind, trust level, signer and record id of the expected shape;
- the verifier's tree equals this run's own tree and its check equals this job's check;
- the record's time is not in the future;
- **no file that decides skipping differs between the accepted base and this run's tree**:
  `scripts/ci-evidence-gate`, `scripts/evidence-verify`, `.st/evidence-skip-policy.json`,
  `.st/evidence-keys`, `.github/workflows/fleet.yml` and its `.genie.ts` source. Such a change always
  runs everything, and a comparison that cannot be made counts as a change.

A verifier that is missing, slow (30 seconds), crashes, prints invalid UTF-8 or contradicts the run
leaves `decision=run`.

**The gate script comes from the accepted base, like the verifier.** The step runs
`git show "$BASE_SHA:scripts/ci-evidence-gate"`, never the checked-out copy, and if the base has no
gate or it fails in any way the step reports `unavailable` and the check runs. A change that rewrites
the gate therefore cannot make itself skip, and the protected-path rule above makes any such change
run in full. What remains is the workflow file itself, which a same-repo change can edit; that is
visible in review, is itself a protected path (so that very change runs in full), and is the documented
same-repo limit. Fork pull requests never skip.

Limits, stated plainly:
- The shipped `observed` rule has no `required_inputs` or `bind_inputs`. That is deliberate: a note is
  keyed to the exact tree, and the tree contains the workflow, so tree equality plus the pinned command
  and runner prefix is the binding. The central observer additionally records full tier only for a
  workflow and recipe matching its operator pin.
- For a pull request the tree is the merge-ref tree, so a note matches only a branch that is current
  with main.
- With no age limit (`max_age_s` null), an old success on the identical tree can skip checks that depend
  on the hosted image or on timing (`typescript-client`, `isolation-vm`). That is a choice, recorded in
  the known defects; `age_s` in the decision line is there to audit it.

### Skips are not recorded as fresh runs

A job that skipped because a note covered it still ends in success. Each gate job therefore runs one
more fixed step, `evidence skip taken`, when it decides to skip. **The observer must not record a check
whose gate job ran that step as a finished run**: otherwise one real run would keep refreshing its own
skips and the age and attempt data would lie. This is a contract with the observer (it lives in
`.github/workflows` of the Evidence observe workflow, not in this change) and nothing may consume the
skip decision until the observer honors it.

### The decision line

Every run prints exactly one line, to the job log and `$GITHUB_STEP_SUMMARY`, with no spaces inside
values and `-` for none:

```
evidence-decision v1 check=… decision=skip|run outcome=satisfied|degraded|no_note|rejected|unavailable
reason=… record=… signer=… trust_level=… kind=… tier=… observed_at=… age_s=… max_age_s=… attempts=… enforce=true|false
tree=… head=… base=… event=… pr=… repo=… run=… attempt=… fork=true|false verifier_sha256=… gate_sha256=…
```

(one line; wrapped here). Each job also ends with exactly one of these steps running, which is how a
collector reads the outcome from the jobs API: `evidence satisfied by existing note`,
`evidence existing note is degraded`, `evidence no note`, `evidence note rejected`,
`evidence verifier unavailable`, plus `evidence skip taken` when the decision is skip. A note made for a pull request is reused by the merge queue's run
only when the tree is identical, that is, when main has not moved in between.

### Loops

No workflow has an unfiltered `push` trigger (the `push` triggers are limited to `branches: [main]` or
tags), so pushing `refs/notes/evidence/*` starts nothing; a test fails if one is added. The gate holds
`contents: read`, so it cannot push, dispatch or comment and cannot cause a rerun. The record's
`attempts` count (trusted records for the tree and check) is what a producer reads to refuse a
second production.

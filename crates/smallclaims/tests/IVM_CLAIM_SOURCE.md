# Retained admitted-claim installation adapter

This isolated opt-in adapter connects `ViewRuntime` local appends and bounded replicated
projection to the namespace-aware installer. It uses the runtime's existing claims-only
admission policy; it does not replace st3 typed admission, arrangement authority, fleet
signer closure, owned desired sets, local numeric observations or the ordered run-tree fold.
The nine controls have passed in local iteration using the standard Cargo wrapper
and target runner with default jobs and test threads.
Hosted CI and source review remain required before merge.

Construct `ClaimSource` with the shared `Installer`, a source name and 1–32 exact kinds,
then construct `ViewRuntime::with_claim_source`. Opening creates only local adapter tables
and triggers. It does not register a source, build an index over history, extract history,
start a job or publish a root. Explicit `register` attests exclusive runtime/hook ownership
at a fresh Store admitted/projected cut. Existing claims remain in the retained source and
are visited only after explicit `Installer::start` and `ClaimSource::extract` pages.

Local appends and replicated projection call the same transactional capture seam. Declared
kinds are dispatched before adapter SQL; unrelated kinds do not change installer source
revisions or semantic roots. Each relevant claim becomes an idempotent complete replacement
keyed by claim ID, with subject/actor/origin/body/predecessors and the exact canonical sortable
tuple. Decimal time preserves u128; arrival index is never a winner rank. This source retains
repaired originals and replacements when present in admitted `claims`: record state does
not filter its relation. A consumer requiring replacement-only eligibility needs a separate
versioned source. Actual accepted repair flow remains an execution/integration gate.

Missing registration is deferred without rejecting a claim. Fenced sources continue source
revision progress without payload extraction or callbacks. An operator failure rolls back
its namespace mutation and fences that root while preserving admitted claims. A new explicit
bounded installation can recover the root; source coverage/version/canonical gaps additionally
require `restore` with the exact current source position. No automatic restart occurs.

Extraction uses the existing `claims_kind_index(kind, store_index)`. It merges at most
`kind_count * (page_rows + 1)` integer candidates, where page rows are 1–128. Point payload
reads inspect stored bytes before decoding and the encoded page stays at most 1 MiB.
The caller captures rows, cut, cursor and source position in one short read snapshot, releases
it, then submits the page to the installer in a separate writer transaction. The cursor is
fixed-width arrival position, valid only in that source lifecycle. Concurrent admitted claims
are captured in the installer journal; publication is still one atomic namespace switch.
An oversized single fact fails extraction and leaves output unready: the caller must cancel
or choose a reviewed bound, never silently skip it. Callback/commit timing is unmeasured.

Canonical record changes discover sources by indexed claim ID/kind. A captured position
cache lets ordinary record insertion compare the current indexed MIN without adding another
legacy COUNT scan per record. Missing provenance in already projected history conservatively
fences. Uncaptured claims beyond the projected frontier remain pending and acquire their
canonical rank during projection; reads remain unavailable until that projection completes.
The v2 adapter fingerprint fences prior registered namespaces on upgrade, and replaces only
the bounded record-insertion trigger definition. Unchanged captured
positions captured after registration preserve readiness. Record position/reassignment/delete, direct claim mutation or
deletion, batch canonical metadata changes and source-epoch changes fence roots and stop jobs
transactionally. Deleting an unrelated kind can alter legacy ranks, so deletion conservatively
fences the finite registry. Repair state alone does not change this retained relation.

Extraction of claims written before registration does not seed the capture rank cache.
The exact populated-before-register → install → ordinary-seal schedule therefore fences
source availability even when the historical claim's canonical tuple and answer are unchanged.
The historical sealing control asserts missing provenance, unchanged canonical rank, admitted
frontier, namespace facts and semantic generation, changed availability/error, and successful
later admission while output stays unready. It performs no read-triggered or startup cache
seeding. The older sealing control covers registration before append only; it is not general
populated-store sealing closure. Historical provenance seeding/recovery remains a separate
bounded source-install implementation and cost gate.

Database and transaction failures propagate; they are not swallowed like an operator callback
error whose namespace savepoint can be rolled back while source admission commits.

The adapter does not eliminate `canonical::claim_key`'s existing legacy COUNT cost. Rank
provenance and operator candidates grow with retained declared history, and orphan provenance
has no bounded reclamation API yet. Source/namespace/journal/provenance writer time, retained
bytes and unchanged-answer growth must be measured before adoption; the b563 rejection stays
mandatory. No growth or writer-held budget is qualified by source inspection.

Read `ClaimSource::root` and namespace output in the same authoritative snapshot. It checks
Store admitted/projected freshness in addition to installer epoch/fingerprint/revision.
`availability` composes shared source pending/ready sequence, install status revision, cut and
semantic generation. It is readable while pending/fenced; it never grants authorization or
witnesses durable action events. Subscribe-before-snapshot/recheck and commit-only notifications
remain consumer/graph-watch integration work. A bare `Installer::root` is insufficient for this
Store adapter because it lacks the enclosing admission frontier.

Signer metadata, external file identity, deadline watermarks and local replacements are not
this source's inputs. Operators must not infer them from raw claim fields. No signature-only
fleet recovery is claimed. Exclusive file/runtime ownership remains a prototype precondition;
raw writes outside the listed mutation triggers, alternative runtime admission, restore/trim,
retention, capability/layout review and complete hook coverage remain production gates.

Nine mailbox controls exercise real Store writes/reads, signed-envelope replication
in forward/reverse message order plus duplicate delivery, missing populated initialization,
interleaved extraction/journal catch-up, unrelated unknown kinds, source pending availability,
callback failure preserving admission, explicit bounded recovery, rollback, disk reopen,
payload bounds, remap fencing, post-registration captured local sealing versus changed record
position, and the conservative historical-cache sealing schedule above.
The raw retained-history fold exists only in tests. Direct SQL corruption controls establish
fencing; they are not accepted repair/retention lifecycle proof. The separate 55 controls also passed at that head within their documented restricted
input policies; these results do not prove complete production authority or cost. No production runtime, route or source hook has been activated.

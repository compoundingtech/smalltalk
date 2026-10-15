# SLO breach obligations and missions

This first source change adds the canonical transition and ordinary mission-opening
seams. It starts no evaluator, timer, collector, profile, network publication or
runtime writer. The standing owner is `agent/fleet/smalltalk/slo`; the existing
implementation claim remains `01a1413009c97c3096561687d4993f8b/implement-source`.
The source anchor is `5ab05fb7a3b28cf6ecb4f99ec17583b76078f731`.

Nathan accepted `ask-d417200c75d57ef07ea5738b844c59e5`: **“Yes, within these
ceilings.”** The posted ceilings are 64 objective slots per member, two mission
openings per minute, 16 claims and 32 KiB per group, and design maxima of about
32,000 claims and 60 MiB per member per day. Those are design ceilings, not measured
cost or activation approval. There is no objective or threshold mutation here.
The accepted direction remains combined daemon plus actual workers at most one
core per member and positively eligible full-hour idle CPU at most 0.5 core.

## Authority and episode identity

`slo.breach-transition` is system-only, append-only and envelope-signed by the
observing member. The actor is `daemon/runtime`; the record's logical member must
match the claim origin and subject, including the reading's member/epoch/sequence
and predecessor cut. Existing envelope admission binds that origin
to the authenticated writer. Each node validates the bounded typed record before
projection; an origin signature attests provenance, not independent OS truth.
Older registries retain the new kind as UNKNOWN and do not publish its facts.

An episode key hashes logical fleet, member, objective and the predecessor recovery
claim. It does not include a credential, PID or build. Ordinary credential rotation
and daemon restart therefore do not mint another episode. A later episode needs the prior recovery and a genuinely later qualifying end cut.
Its rolling window may overlap the preceding healthy window. An older recovered
undispatched episode keeps its own durable mission obligation; the two are not
collapsed or lost on restart. Conflicting same-event contents, conflicting equal-ordinal heads, missing
lineage or incomplete first sync fail closed. A restore with uncertain lineage
must not be activated as a fresh logical member. This seam does not certify restore
lineage or invent an independent genesis certificate.

The producer also keeps a bounded local observation fence under the writer: latest
sequence/cut, latest qualified miss and the end of unknown coverage. Met and unknown
samples update this memory only. A late healthy cut cannot close after a newer miss
or unknown interval. Recovery carries the latest missed cut and coverage floor
inside the existing bounded signed record. Restart loses local coverage and creates
a fresh epoch; a complete new window after that epoch is required, while canonical
episodes and pending missions remain. Malformed/equal-sequence conflicting input
invalidates that objective's source epoch. This local fence is not historical
closure authority: only the explicit signed recovery is projected by replicas.

Records are transitions only: pending first miss, dispatch/owner, first owner block,
first owner unblock and recovery. There is no heartbeat or sample claim. A missed
reading carries objective version, target, units, population, exact window,
coverage, source cut and a bounded existing counter cut. Missing and partial
coverage is UNKNOWN. Recovery needs the same effective objective and a complete
healthy window after the onset; it does not complete or cancel an admitted fix.
A busy member with no eligible idle hour remains unmeasured for that objective.
Maintenance, heartbeat, observation and anti-entropy CPU always count. Foreground
activity, sync catch-up, deploy or unknown eligibility prevents an idle PASS.

CPU records include bounded actual process identities, roles, births and monotonic counter cuts. The witness cut and
UTC endpoints must equal the reading's, and its monotonic duration must match that
millisecond window. A two-hour average cannot establish an idle hour. The declared
nonzero worker count must match all supplied worker roles; a daemon-only or unproved
zero-worker topology remains UNKNOWN in this source seam. Both miss and recovery must cover the full window. Counter resets,
duplicate identities, unknown cohorts, missing births and uncertainty crossing a
target boundary are rejected. The origin must still establish that this is the complete actual cohort. OS
credentials, executable/role verification and real worker lifecycle binding remain
producer/activation holds; a count or signature alone does not prove them. No existing 60-second process sample or worker lifetime
counter is promoted into an hour of combined coverage.

## Preparation and ordinary work admission

The three shared projections are `slo_pending`, `slo_episode_heads` and
`slo_episode_events`. Event selection uses minimum claim id and preserves a conflict
witness and maximum-ordinal witness; it does not depend on receipt order or store
indexes. Explicit record repair refreshes that episode from its canonical sources;
an objective index selects the surviving head without replaying other episodes. Opening a store only
creates empty tables and indexes. Explicit import/replay reduces canonical sources;
replicas never originate missions.

Preparation reads at most two oldest local pending obligations and parses the
fixed two-step mission outside the writer. Its caller supplies a bounded absolute
workspace explicitly; this seam does not invent a relative checkout or inspect the
host filesystem. Publication revalidates the exact onset
claim, episode head and member credential under the existing writer, then opens one
ordinary owner-assigned mission in one transaction. The default owner is
`agent/fleet/smalltalk/speed`; an explicit owner is retained. Owner lookup is a
point read. The source consumes the existing membership certificate only when its
fleet generation is current; it does not trigger a membership fold or snapshot
build. The same generation, fleet binding, first-sync state and projection health
are revalidated by point reads under the writer. Normal seat capacity and queue admission still apply to execution;
this code never calls `start_work` as the owner or chooses a person/CoS fallback.
An absent owner remains named pending work. Later owner flaps use ordinary owner
facts, with at most one block and first-unblock receipt per episode.

The mission id is `slo/EPISODE`. Its agentless dispatch step uses two existing Field
gates: `record.episode is EPISODE` on the immutable
`resource/slo-breach/EPISODE@ONSET_CLAIM_ID`, then `dispatch_state is ready` on
`resource/slo-breach/EPISODE`; its fix depends on that step. An older registry does
not admit the onset claim, so its ordinary pinned-resource lookup is absent even
if an unrelated ordinary observation names the same resource. No new gate opcode
or old-selector behavior is assumed. The mission publication cites that onset through
existing evidence metadata, so checkpoint reference guards retain its exact ID even
if a later duplicate changes event selection. The dynamic fact is derived on each node by the episode reducer, never a
`resource.observed` claim. Gate reads use a short source snapshot and fixed indexed
point reads. The real work selector and claim path also require the intrinsic
positive execution-admission receipt for a newly ready fix. Breach status is
separate from this durable readiness fence. Initial owner absence is encoded in
the dispatch receipt and needs the first owner-unblock receipt before admission;
receipt order cannot bypass that block. Once admitted, the gate stays positive
through healthy recovery, a later episode and ordinary owner availability changes.

If recovery precedes dispatch, the one mission still gets created, with
`execution_ready=false` intrinsically stored in its dispatch transition and an
ordinary explanatory cancellation. No prefix containing only mission, onset and
dispatch claims can become executable while waiting for recovery or cancellation.
A dispatched repair obligation that initially lacked its owner is retained through
recovery; the first unblock remains permitted when that owner returns. It then follows
the ordinary Field/dependency/claim path. No healthy result is a repair completion.
An already admitted fix keeps its positive receipt after recovery or a later breach;
its active claim, substantive work and history are preserved.

## Bounds, replay and retention

There are at most 64 objective heads. This is not a 64-row lifetime-history cap.
Older recovered episodes retain undispatched
obligations until their deterministic missions are published. Reads prepare at most
two oldest obligations. Mission openings are capped at two per rolling minute and
700 per rolling day. Canonical intake additionally caps new onsets at 832/day and
first-unblock receipts at 700/day, so delayed owner return cannot bypass the write
budget. An exhausted budget returns an explicit source error; this code starts no
retry loop or commit-triggered evaluator. Capacity/coverage failures must remain
visible in the later producer/read integration before activation.

A record is at most 8 KiB; its reading/counter summary is at most 1 KiB. A receipt
has a conservative signed/base64 framing bound of 16 KiB. A mission-opening group
is at most 16 claims and 32 KiB with signature, batch and base64 allowances. A source emits at most one receipt of each kind per episode. At most 64 previously
open episodes can recover in addition to the 832 new onsets, and the 700 first
unblocks are separately bounded. The resulting source ceilings are 700*32 KiB plus
(832+896+700)*16 KiB = 59.8125 MiB and at most 13,628 claims/day. Administrative
repairs and later substantive work require their own qualification; these sums do
not measure whole-system write or CPU cost. This arithmetic is not writer, replica
CPU, memory or elapsed-time proof. The actual signed-envelope control checks wire
size; the first hosted cost control measures full writer and replica holds plus test-process
CPU deltas. Nextest runs each named control in its own process with retries disabled
and successful output retained; those deltas include both the
calling thread and the actual store writer thread. They do not establish production
or hourly idle coverage.

Checkpoint rules version 16 includes all three tables in replay and shared graph
proofs. It keeps onset, dispatch, owner and recovery fences, including their signed
payloads and a conflicting body where needed. Only redundant receipts of recovered
episodes can compact once the recovery is at least seven days old and existing
reference/whole-envelope guards permit it. There is no seven-day total-storage
promise: the approved retained fences and ordinary mission/work history persist.
No startup history scan, local historical-closure authority or destructive migration
is added. Empty new tables do change projection root bytes. The existing transport
compares graph roots only for the same registry: mixed registries wait with UNKNOWN
comparison status instead of treating those roots as a proven divergence. Fresh
checkpoint verification requires every participant to agree the same rules digest;
version-15 and version-16 terms cannot form one complete certificate. Existing
historical-certificate adoption keeps its existing proof policy. Roll forward and
coordinate participant upgrades before creating new version-16 checkpoints. The
curator confirmed these consequences are within the same accepted ask, with exact-head
mixed-digest UNKNOWN and mixed-rules refusal controls required green before merge.

## Qualification and remaining integrations

Every new control is **UNRUN** until the exact delta is admitted by Intake and an
ordinary hosted run reports results. Local Cargo, native runs, load, host probes,
profilers, runtime activation and deployment are outside this implementation claim's
authority. The writer-hold number must be reviewed next to st2 #3080's measurement;
shared-replica CPU and worst-case transition cost are later GO/activation gates.

The prefix control declares a live owner/incarnation independently of the imported
group and deliberately omits cancellation, so a terminal run cannot mask readiness.
It preserves original claim IDs and enumerates every subset of the finite known/new claim group,
forward and reverse receipt order, current and old registries, checkpoint replay
and reopen. It calls the actual Field evaluator, work selector and normal claim.
The separate signed-exchange control checks old-registry retention and upgrade.
Missing ordinary prerequisites may defer projection; they must remain non-executable.
The old-registry path relies on the ordinary immutable-source Field predicate remaining
Pending when the onset is unadmitted, followed by the reducer-derived readiness predicate;
the raw fix prerequisite stays pending
before the new ready-only selector check is reached. A matching positive fixture
drives the ordinary gate, dependency and successful claim. Neither fixture constitutes a deployment or independent measurement of production.

The future producer must reuse existing bounded SLO evaluations and #3074 accounting,
with #2930/#3080 writer observability first. This PR does not adopt those unmerged
heads or infer their approval. Fleet snapshots extend #2916 `slo.list/history` only
after the separate PUBLIC CONTRACT review of fields, limits and cursors. No fleet
selector, public route, policy/config or new sampling framework is in this PR.

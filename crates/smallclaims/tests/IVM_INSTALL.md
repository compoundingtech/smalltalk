# Explicit installation prototype

`ivm::install` implements an opt-in installation coordinator and a separate namespace-aware
operator interface. It is not connected to `ViewRuntime`, production Store writers, doctor,
checkpoint, subscriptions or any application route. Existing unscoped `View` callbacks cannot
use it without a source adapter and a namespace-aware port. All 13 installer tests have passed
in local iteration; the opt-in claims-only
adapter is exercised separately. Production lifecycle and timing remain unqualified.

The source adapter must supply a versioned, complete retained input relation, an indexed
stable-key extractor, and full old/new replacements in the admitting transaction. Its
fingerprint includes the input registry, eligibility, canonical ordering, authority, reverse
dependencies, external/local identities and extraction rules. Source registration is an
explicit owner attestation, not a capability proven by this module. Signing, typed claim
admission and exact canonical tuple calculation remain source-owned. A new source lifecycle
cannot silently reuse an old revision/namespace.

For each admitted replacement/retraction, repair/re-admission, actor/owner reassignment,
record-rank/signature mutation and relevant local change, the adapter must call `record`.
A declared source includes every dependency that affects its output; it may exclude genuinely
unrelated kinds. Local clock/deadline progress also needs an explicit source event/watermark.
Calling `source_gap` on incomplete capture fences installed output and stops extraction.
Source writes continue while coverage is unavailable. Only explicit `restore_source`, followed
by a new complete installation, can publish again. Neither a source revision nor a root token
supplies authorization to act.

Installation begins only with `start`. It creates a private namespace and durable cursor,
retained-source identity, baseline revision and limits. An existing compatible root continues
ordinary scoped maintenance while its replacement builds; a missing/incompatible root remains
unready. The source adapter captures each extraction page and its position in a short read
snapshot, releases the snapshot, then submits the page in a separate writer transaction.
Pages must visit stable keys in strict byte order. All extraction finishes before journal
catch-up begins. Full replacements are idempotent: ordered catch-up corrects scan rows that
observed concurrent changes, including insertions before the scan cursor and deletion/re-add.

`scan` and `catch_up` use savepoints. A failed callback cannot commit partial namespace rows,
cursor progress or consumed journal state. A successful page commits all of them together.
`catch_up` publishes only at the current source revision with zero outstanding counters and
the operator's bounded completeness check. Publication changes one root pointer; it performs
no table rename, mass row copy, per-key invalidation loop or old-namespace deletion. Readers
capture `root`/`status` and actual namespace output in the same authoritative snapshot. Saved
namespace pointers do not establish historical pagination or immutable content cuts.

Journal storage is shared per source, with one entry per admitted mutation while a build is
active. Both queued and total retained journal rows/bytes are capped. Quota/oversized-entry
failure stops derived jobs without rejecting source writes. Consumed entries still count
against the retained cap until an explicit bounded `prune_journal` page removes them. The
indexed minimum active consumed revision protects all builds. Total installation work and
wall lifetime are also bounded; overload stops rather than automatically restarting forever.
No third namespace starts until a detached prior namespace is explicitly reclaimed. Reclaim
callbacks must delete at most the supplied row count across all namespace-owned tables.
An incompatible old schema requires its compatible reclaimer, not an open-time cleanup.

`progress` exposes cursor, phase, error, backlog and extracted/applied row counters.
`status` remains readable when fenced. `cancel` is durable. The caller must schedule/yield
between pages, provide commit-only notifications, expose status/cancel through doctor, and
compute an ETA from measured throughput; no scheduler or truthful ETA is implemented here.

The callback duration limit is checked after the callback and before releasing its savepoint.
It can roll back and stop an over-budget page, but cannot interrupt a slow SQLite statement,
callback or commit/fsync. `max_page_us` is pre-commit page elapsed time, not a full writer-held
measurement. A production tens-of-milliseconds writer budget, page rows/bytes, WAL impact,
ordinary-write interleaving, journal cost and total/maximum writer-held times on a copied 2 GB
store must be measured. The tests use a generous correctness-only callback budget. No timing,
growth, authority or production readiness acceptance is claimed.

The fixture is a normalized admitted mailbox fact relation. It maintains namespace-owned
message rows and recipient unread counters with indexed point changes and compares them to
an independent raw-source SQL oracle. It covers interleaved replacements, removal/re-add,
before-cursor insertion, duplicates, source/derived rollback, reopen, missing journal events,
operator errors, quotas, source coverage loss, version/clock fences and explicit recovery.
It does not prove real mailbox signing/admission, current numeric historical-hour selection,
owned desired-set closure or application Store hook completeness. A private corruption
control also verifies that a root cannot alias another source with an identical fingerprint,
epoch and revision: reads/status and live maintenance bind to the declared source name.
The separate 23 real-view and 19 primitive controls have also passed locally
within their documented input policies.

Checkpoint trim/adoption and cross-handle protection are still unsupported for registered
production views. Retained-source completeness, mutation hooks (including SQL-only changes),
layout/rules decisions, live commit availability transport, peer artifact validation, executed
exact-head tests and writer/growth measurements remain required before production wiring.

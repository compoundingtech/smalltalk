# Committed view invalidations

`smallclaims::ivm::events` is an explicit, local event surface for registered `Views`.
It installs neither a daemon route nor graph-watch transport. Register the feed with
`events::install` in an explicit writer transaction after creating/initializing views.
It establishes a database identity and starts at the current frontiers without scanning
old keys. Existing consumers need an authoritative initial snapshot, not cursor zero.
Neither ordinary open nor a GET installs it.

Key updates record `ViewChanged { view, key, key_generation, sequence,
recorded_semantic_generation, source_cut }`. Availability updates record
`ViewAvailabilityChanged`, including shared source invalidations and current fenced error
evidence. Event rows, output, source cut and generation roll back together. The returned
payload's source cut is the current committed cut in the same read snapshot as the page,
not a historical answer for each event sequence. Removal and re-add invalidations remain
distinct positions, but clients re-read current state; this is not an action occurrence log.

Attach one `Publisher` per Store for the transport bridge. It uses the existing writer
commit observer, reads one fixed metadata query after each successful batched commit or
returned lent writer, and broadcasts a bounded notice. Rolled-back/no-op writers whose
metadata did not change emit nothing. No SQLite callback hook, source-body scan, or network
delivery runs in the callback. Graph-watch owns client fanout and authorization.
An unfinished transaction returned by a misused raw writer emits `Unavailable`, never a
committed frontier; lent callers must finish their transaction before returning the guard.

The bridge subscribes **before** its authoritative read snapshot. It captures `Boundary`
along with the predicate/output, releases that snapshot, and only then awaits a notice.
A commit before the snapshot is reflected by the durable boundary; one after it stays
buffered on the receiver. A lagged notice means recheck durable cursors and floors.
It does not itself prove that retained changes are missing. Publisher drop closes its
receivers and unregisters the observer; tie publisher lifetime to the Store/bridge.

`ProviderIdentity { database_id, view, fingerprint, epoch }` is stable across semantic
updates. `ChangeCursor` adds an independent key or availability stream sequence.
`SnapshotVersion` holds semantic generation and availability evidence for a page snapshot.
Use `expected=None` to reconnect to current invalidations with the stable identity; pass a
captured snapshot version only for generation-fenced continuation. A changed snapshot
returns `SnapshotChanged`, while wrong database/version/epoch, expired history, wrong stream
or a cursor ahead of its source returns `Resync`. These are different outcomes.

Both journals retain at most the configured number of sequence positions (1–4096).
Each insertion advances an explicit inclusive floor and deletes only the indexed expired
prefix. A cursor equal to the floor may continue; one below it has a gap. Pages return at
most 1024 rows. This retention is global per stream: unrelated view traffic can expire an
old cursor. Original contribution, rank and key-token tables still have their separately
documented retention limitations; journal retention does not bound those tables.

Journal keys are limited to 4096 bytes and view names to 1024 bytes. Exceeding these limits
fences the event feed without rejecting the admitted source or truncating its key. A
publisher emits `Unavailable`; output readiness/authorization must still be checked
separately. Event installation is not production source-coverage or checkpoint certification.
Restore/clone owners must explicitly rotate database identity before exposing the file,
after certifying source lifecycle; rotation cannot heal a missing/oversized feed or output.

Installer-owned namespaces outside `Views::maintain_key` need an explicit keyed publication
adapter. Numeric, local deadline, authority and external-source changes likewise need their
declared old/new dependency adapters. They do not gain coverage by attaching this publisher.

The reference runtime now rejects `ReplaceOriginal` views until repair-before-projection
eligibility has a proved implementation. The general engine retains its explicit caller-owned
repair API. Historical retained-claim installation still lacks rank-cache seeding; its next
ordinary sealing can fence the source, as documented in `IVM_CLAIM_SOURCE.md`. Both are gates
before consumer adoption. Database errors now propagate out of view savepoints instead of
persistently fencing a view, and pending projection fetches use `LIMIT chunk + 1` before
decoding. None of these changes activates a production reader.

Run targeted controls through normal Cargo configuration and the configured runner:

```sh
cargo test --locked -p smallclaims --test ivm_events \
  --test ivm --test ivm_install --test ivm_real_views --test ivm_claim_source
```

Read-after-write waiting and asynchronous view catch-up are the next ordered mission steps.
An invalidation sequence, semantic generation or maximum applied claim index is not a
certified read-after-write prefix. This event surface makes no such promise.

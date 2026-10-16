# Private resource observation preparation prototype

This successor implements Root's native-fence direction and exact two-attempt
progress decision (`message/33b0e7227461f7d0` and `message/36b5b6346537f193`)
on installed source `4b04f5bdbefb9d7ecac427711a4449866a64f03e`. It supersedes
the blocker-only implementation at `67001b07`; its four counterexamples remain.
All 21 authored controls are **UNRUN**, including first compile. Static source
comparison, rustfmt parsing, diff checks and JSON parsing are the only checks
performed. No local Cargo, SQLite, compiler, live read, profile, CI action,
publication or runtime adoption occurred.

The source trace remains
`doc/fleet/smalltalk/speed/2026-10-16/writer-resource-source-trace@479ded00abd546c1cf6dfa1632ccc781ceffa509b15dfe3760ce41c459e27a39`.
Its 03:20:22.190Z event identifies a 9,951.192 ms batch with a 9,869.132 ms
longest resource-observation job. It does not identify the resource or expensive
inner operation. The original method is identical across ae128a22 and 4b04.

## Algorithm and effects

`resource_preparation::inputs` extracts the original canonical prefix: active
subscriptions, normalized facts and attribution, canonical resource and
collection history, observer health, recipient existence, subscription state,
and the no-change decision. It reads the canonical source, including repaired
claims and ordering inputs, rather than trusting the latest resource projection
or its pending marker. Registry inputs are this executable's immutable registry.
The three helper parameter changes from Transaction to Connection are read-only;
their bodies are unchanged.

Each of at most two attempts first borrows the existing writer at autocommit,
reads its native cut and returns the loan. Then it starts and ends a fresh reader
snapshot, prepares inputs, releases every guard/pin, and queues a normal batched
job. That job retains the original completion/revision and idempotency checks.
Before consuming any prepared value or error it compares a new native cut on the
actual transaction connection. Staleness has no effects and requests another
preparation. Two unusable/stale attempts select exactly one invocation of the
canonical writer path. There is no busy error or unlimited loop. A previously
cached response can return without consuming the prepared inputs, as before.

The append/delivery/idempotency suffix is the original implementation, with only
its return wrapped in Option. It preserves claim versions/evidence, operation
identity, attempt receipts, notification contents/recipients and authority.
The original completion predicate still runs at both writer boundaries. The
first normalization, collection/history reads and no-change determination move;
changed observations still perform append-time normalization, author attribution
and existing inline custom/resource maintenance in the writer. The fallback
still performs the full old prefix there. Neither a writer-hold cap nor an outage
cure follows from this prototype.

## Native cut and coverage

The cut retains native connection identity, filename, total_changes64, main and
TEMP schema cookies, main data_version and user_version. Store identity uses a
retained Arc allocation belonging to this CoreStore, not the numeric committed
claim frontier. Prepared values additionally belong to one Fence instance. The
original Store remains borrowed throughout the operation; its writer is never
closed/replaced by this path. Reopen allocates a new incarnation while the old
Arc remains retained, so raw-address reuse cannot validate it.

Pinned dependencies are rusqlite 0.32.1 and libsqlite3-sys 0.30.1, bundled SQLite
3.46.0. The bundled feature selects modern_sqlite, and InnerConnection's wrapper
calls sqlite3_total_changes64. The following facts were checked in primary docs
and the pinned wrapper/amalgamation source:

* [total_changes](https://www.sqlite.org/c3ref/total_changes.html) counts completed
  same-connection row mutations, including trigger and foreign-key work. REPLACE
  omits its implicit deletion but its successful insertion counts. A synthetic
  INSTEAD OF view row is omitted; its actual base-table writes count. The
  prototype uses no update hook and therefore does not inherit its WITHOUT
  ROWID or truncate-delete exclusions. Rollback of completed DML conservatively
  invalidates; the authored control requires this pinned behavior.
* [data_version](https://www.sqlite.org/pragma.html#pragma_data_version) is compared
  only on the same native writer handle. It detects other-connection/process
  commits; it is not evidence for own-connection writes. The in-transaction
  total-changes comparison detects earlier same-batch DML before publication.
* [schema_version](https://www.sqlite.org/pragma.html#pragma_schema_version)
  catches ordinary DDL and VACUUM. The pinned VACUUM implementation restores its
  saved row counter but increments the schema cookie. A schema cookie is never
  treated as file identity. Native read errors and a signed-counter overflow
  value make a plan unusable.
* [SQLite isolation](https://www.sqlite.org/isolation.html) protects the later
  transaction: a WAL read cut cannot upgrade after another writer has made it
  stale. Existing canonical transaction errors remain errors, not fast-path
  success. No uncommitted reader mode is admitted.

This is a conservative connection-wide fence, not keyed invalidation. Supported
raw DML, repair/import, row deletion, order changes, pending projection changes,
observer/recipient/opener changes and trigger work need no separate registration:
the same writer's actual counter is read before use. Neither observe_commits nor
projection_digest_generation participates in the proof. No hook, dispatcher,
transaction-finalizer, telemetry label or schema callback is installed/replaced.

Coverage is for this installed repository's ordinary physical mutation paths,
not arbitrary new SQL extensions. The source audit found no production assignment
of schema_version/writable_schema, native deserialize/backup-into-live-store,
writer-handle replacement, or RAISE(FAIL/IGNORE) trigger. The latter matters to
REPLACE: no unsupported abort-after-implicit-delete path is assumed covered by
its omitted delete count. Cookie forgery, custom native functions that mutate
physical pages outside SQLite DML, an externally replaced live file, and unknown
custom mutation APIs are not proved safe by these numbers. If any is a supported
production path, this candidate needs an additional native fence or eligibility
proof before admission. This is an explicit review obligation, not a newly
granted restriction on an existing caller. Logical caps and public errors are
unchanged. The authored REPLACE control covers the actual ordinary successful
REPLACE shape, not every possible custom trigger program.

## Store, snapshot and loan eligibility

CoreStore::open opens one native writer, normalizes its opened filename, and
constructs ReadPool from that same path. open_memory constructs one unique UUID
shared-memory URI for both; its native empty filename is accepted only for that
Store's shared-memory mode. Preparation checks Store/ReadPool path agreement,
reader native filename against the seed, and the reader's main schema cookie.
It rejects foreign Fence/Store incarnations. It does not compare data_version
between connections.

An existing request loan or registered pin is ineligible; an unregistered active
reader transaction is also ineligible. After the seed is returned, read_snapshot
performs BEGIN and a first current_index read before the input closure. It
cannot join an old same-store request/pin because those were excluded on the
same thread before seed. Its guard rolls back/releases on every exit, before
validation is enqueued. TEMP objects could change name resolution differently
on the writer and reader; the private path permits only the write_clock table
and the retained heal_replay_base_order table used by canonical replay. Neither
name shadows a preparation dependency. File-backed initial open runs that replay
and retains its empty scratch table; rejecting it would disable the optimization.
Eligibility compares the stored CREATE definitions with the two actual source
schemas after collapsing whitespace, not names alone. The replay table must also
be empty. Mismatched columns, extra columns, views, triggers, indexes, unknown
names or retained replay rows select the old path. The [SQLite schema table](https://www.sqlite.org/schematab.html)
describes stored CREATE text; the pinned bundled sqlite3EndTable implementation
constructs it from the object-name token, omitting TEMP and IF NOT EXISTS.
The catalog check and conditional scratch-emptiness query are additional SQL
cost, not free metadata; later schema changes still invalidate the native cut.
The file-backed/reopen control requires an actual fast observation, rejects each
unexpected TEMP shape/state, and proves unchanged canonical fallback appends no
claim. It does not weaken the original reopen/store-incarnation refusal.

The actual production call is reconcile's async observer completion, after the
provider await and prior reader guards have ended. It holds no writer loan,
pinned read or scheduler mutex. Its current predicate releases the scheduler
mutex before returning. The unchanged method already enqueues batched work and
cannot recursively be called while owning its writer; native write/batched also
retain their existing no-pinned-write assertion. This candidate adds no support
for such recursive calls and does not probe for ownership by taking that loan.
The ambient unpinned request control instead demonstrates supported ineligibility
and one canonical fallback without any seed. Other consumers require the same
existing nonrecursive write precondition; a newly discovered supported loan-held
caller would be a blocking review finding, not silently reclassified.

Physical restore's existing gate requires an empty offline destination and an
exclusive lock, creates/closes/checkpoints a separate staged Store, then publishes
that file. It cannot replace the active nonempty Store through that supported
path. Logical restore/repair/import uses ordinary writer DML and is conservatively
fenced. The reopen control retains the old incarnation while reopening the same
path. None of this authorizes a restore or tests a running member.

## Cost and controls

A quiet success adds one seed loan, one reader preparation, and one validation
job. At worst there are TWO seed loans, TWO preparations, TWO stale validation
jobs and ONE canonical fallback. Each native seed/validation reads five SQL
statements: TEMP catalog and four PRAGMAs, plus native getters. A retained replay
scratch table adds one emptiness statement at seed and validation; corrected
hosted work reports must include it. Preparation has
its own snapshot/catalog checks and canonical history work. Existing observer
checks, idempotency reads, transaction/savepoint/finalizer/COMMIT/observer work
are still real costs. Canonicalizing facts and computing the operation hash also
repeat across attempts outside the writer. Each unrelated write can invalidate a plan; this design
can add two full preparations before still holding the writer for the old fold.
That outcome is adverse performance evidence, not acceptance by compatibility.

Test-only records separate seed queue acquisition wait, borrower body-through-
return elapsed and traced SQL; reader elapsed/SQL; enqueue-to-callback elapsed;
callback elapsed/SQL; and full batched acknowledgement elapsed. Cached response
returns are counted separately from validated fast successes. Borrower elapsed
includes return work, enqueue-to-callback includes prior batch work, and callback
SQL excludes finalizers/COMMIT/observers. These are not independent measurements
of exact native queue/hold/commit phases. Seed accounting includes the return
path on its caller thread; other-thread native work is not counted there. All
records are cfg(test); no product reporting contract is expanded. A source-only
nextest override retains success output for exactly the quiet/busy, history-width
and differential replay controls in ordinary hosted consumer logs. It changes
neither selection, retries nor budgets. This follows the already installed
roster-control output mechanism; the exact configuration delta requires Intake
review before publication. No numeric result is available before that cohort.

The 17 new complete control bodies live in
`crates/st3/src/store/resource_preparation/tests.rs`; the four original negative
fence/attempt bodies remain in `resource_preparation_fence_tests.rs`. They cover
quiet versus two injected unrelated writes/fallback, history-width 2 versus 64,
ordinary REPLACE/trigger/FK/view/WITHOUT ROWID/delete/rollback, raw repair/order/
pending/DDL, another connection, store/Fence/reopen/path/snapshot/TEMP rejection,
request-loan fallback, original error and cancellation, observer repair/revision,
opener and recipient races, changed/attempt differential outcomes and repeated
idempotent receipt, and real prequeued same-batch mutation. The batch control
requires the jobs actually share a batch and fails loudly if scheduling misses
that condition; no mock commit hint is its authority. The differential control
uses the unchanged extracted canonical path; static suffix/prefix parity is
part of its evidence, not an independent reimplementation oracle. Raw race
injections are isolated controls, not new producer authority or incident cause.

Intake must read the exact complete private delta and bodies before any compiler
or publication decision. #2930's sqlite/telemetry seams and #3082's productive
head/cohort are untouched. This source does not close the fleet incident, accept
a failed rollout gate, release a member's disk hold or grant rollout/activation.


Composition note: the private successor merges exact GitHub main
`d8d80829c1742002472206e0ec1011407991d98a`, read once for this composition.
Incoming fleet cache/generation triggers, test-only transport module, dependency
optimization profiles and the targeted roster-output override belong to main.
Native writer/reader dispatcher files, Store constructors/read_snapshot and this
observation's dependency/helper bodies have no incoming mutation-seam change.
The main merge was clean. This is static interaction review, not runtime result
transfer; the first compile and all 21 controls remain UNRUN.

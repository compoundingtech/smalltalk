# Private native-source controls

This module is private and compiled only for tests. An explicit fixture call attaches it
to an empty, file-backed native Store. Store open, schema creation, migration, Runtime
dispatch and public Doctor never call it. Its guard and triggers exist only on that
fixture database. This is the next dormant source slice after #2139, not an installed
production authority bridge or a maintained-coverage certificate.

The binding is one literal `exit_code is <integer>` gate, one run/generation/step and
one locally selected, unmanaged exec. The fixture uses the native mission and desired
projection functions; the adapter reads their selected rows in the same writer
transaction. Its eight inputs are the mission gate, run, generation, step, selected
desired declaration, selected local runtime observation, complete bounded actual-kind
domain and the fixed capture policy. It does not walk rival origins, owned sets,
signer closure or causal history. Unsupported selection retracts this key; malformed,
over-cap, repaired/checkpoint or owned-set inputs leave a sticky unavailable gap.

Native JSON bodies have a 16 KiB cap, identifiers 512 bytes, the total extracted source
256 KiB, and both the whole exec subject and each relevant complete legacy batch have
a 16-row cap. Header queries use `typeof` and `octet_length`; a separately guarded body
SELECT follows only after the caps pass. JSON structure is checked before serde.
The control checks the bundled SQLite version and actual EXPLAIN byte-length opcode.
No incremental BLOB API, new dependency, CAST or TEXT character-length check is used.

Fixture-only triggers capture relevant INSERT, UPDATE and DELETE and advance both
pending revision and Installer source revision. BEFORE INSERT captures replacement
collisions with recursive triggers disabled. All eleven protected native tables have
a replacement control; the three replica/checkpoint tables are already unsupported
before replacement, and their control isolates capture without restoring readiness.
Changing guard binding fields leaves a sticky gap; replacing the guard row is refused.
The explicit fixture finish supplies old/new facts only after the native writes, and
clears pending after successful Installer publication. Native rows, pending metadata,
Installer root and witness all roll back together. A plain raw connection is refused
by the native projection SQL function requirement; registering the existing native
writer functions permits mutation without the fixture finish. Those writes leave the
reader unavailable until an explicit eligible writer finish; no reader recomputes.

The `INDEXED BY claims_subject_index` clause is exclusive to the private fixture.
It must not carry into a production trigger: a dropped or renamed index would fail
every claim write. Production capture needs a separately reviewed mechanism.

The fixture diagnostic has the Doctor line shape but is not an HTTP/CLI route. It emits
only a proven warning or explicit unknown. It never emits pass. The lifecycle control
models explicit private fences for rebuild/migrate/restore; it does not prove existing
production writer dispatch. The retained guard assumes its fixture triggers remain
installed; arbitrary DDL removal is not authenticated by the reader. Production
capture, reopen/migration lifecycle and the complete dispatch-or-fence matrix still
need a separate bridge review.

The fourteen native controls are named `native_gate_*` in tests.rs. They cover native
selection against the canonical oracle (including later arrival with older time),
all six non-selecting runtime kinds, refusal/recovery, changed declaration/generation,
complete subject/batch caps, header/body guards, native malformed-claim refusal,
malformed/deep mission metadata and wrong-type retained fixture payload,
replacement capture, replacement after extraction, rollback, explicit lifecycle gaps,
0/1024/100000 unrelated rows, raw reopen/nonempty refusal, guard replacement and
default-Store inventory. Registration/setup is separate from measured mutation work.
Work scopes count traced statements and VM/full-scan steps from native DML through
finish, excluding BEGIN/COMMIT and setup/verification. The idle guard assertion is one
statement, zero full scans and at most 40 VM steps. The semantic no-op assertion keeps
the proposed 96-statement/20000-VM/zero-full-scan target, without changing generation
or output rows. These are test assertions, not an enforced production deadline or a feasibility,
latency or cost claim. The first corrected fixture head `8af510cc` executed fourteen
controls: nine initial passes and five failures after three attempts each. It observed
only the size-zero idle finish (one statement, 22 VM steps, zero fullscan steps); the
other measured paths were blocked by fixture failures. Successor outcomes must come
from their own hosted source archive.

At each 0/1024/100000 prior-row size, the control also separately reports one unrelated
claim INSERT (insert_count=1), including native and fixture trigger predicates, and
the following idle finish. Batch setup is excluded. Expected statement counts are one
for the INSERT and one for finish; the per-INSERT, larger-source and full no-op
VM/fullscan numbers remain unmeasured until corrected hosted execution. INSERT retains the 20000-VM/zero-fullscan
refusal target and finish the 40-VM/zero-fullscan target. The output also reports the
unrelated point UPDATE and complete bound-key semantic no-op path separately.

A future production hook must have a zero-SQL no-binding writer flag, prove installed
but unrelated write cost, cover both proposed native hooks plus every lifecycle, and
resolve storage version, canonical audit/digest, old-binary reopen and rollback.
Nothing in this slice activates those hooks or changes #2077/#2091.

# Transaction-owned source maintenance

`Store::install_transaction_hooks` explicitly installs one paired adapter on the Store's
writer. Prepare runs inside each new managed outer transaction, before any source job or
helper executes. Finalize runs before SQLite commits, in that same `&Transaction`.
Prepare can detect preexisting committed raw capture and set the adapter's managed-scope
marker; finalize must not reinterpret earlier source commits as its current transaction.
The adapter clears its marker before commit, and rollback removes uncommitted markers.
`install_transaction_finalizer` remains a convenience with a no-op prepare; it provides
no transaction-start capture proof. It can call existing `Views::change`, `Views::repair`,
`Views::local_change` or `Installer::record` using old/new facts captured by its adapter.
This does not install an IVM runtime, populate a view, or certify source coverage.

For a captured gap that must preserve valid admission, `Views::fence(tx, name, reason)`
marks that registered view unavailable; `Views::fence_all(tx, reason)` fences the finite
shared registry. The output, semantic generations, key cursor and source cut remain
unchanged. Availability/error revisions advance transactionally, including changed
error evidence while already fenced; identical evidence coalesces. Rollback publishes
nothing. This is distinct from `Installer::source_gap`, which protects namespace roots
and jobs. An adapter maintaining both representations must fence both; neither API
publishes a new certified cut, re-enables Ready or initiates recovery.

```rust,ignore
let preparing = shared_source_adapter.clone();
store.install_transaction_hooks(
    move |transaction| preparing.prepare_scope(transaction),
    move |transaction| shared_source_adapter.finalize(transaction),
)?;
```

Install before attesting source registration. Installation acquires the existing writer,
so earlier queued/lent work finishes first. Installing a second hook pair or convenience finalizer fails; there is
one writer-held adapter for its lifetime. Keep source registry and Views ownership shared.
The callback must not acquire the writer, end the transaction, or capture a strong Store
reference that creates an ownership cycle. Source adapters must bound their SQL and CPU;
the callback is synchronous and is not preempted.

For queued writes, prepare runs once before their first job; finalize runs once after
their per-job savepoints have resolved,
then the outer transaction commits. A failed job's captured mutations are rolled back
with its savepoint. A finalizer error rolls back the entire batch and every caller gets a
failed commit acknowledgement. Prepare failures skip source jobs/helpers; prepare writes roll back too.
Storage errors and callback panics propagate as failures;
a caught panic does not kill the writer. Operators should use the existing derived-state
fencing APIs for logical coverage failures that must preserve valid source admission;
returning an error deliberately aborts the source transaction.

For lent writes, `WriterGuard::transaction()` and `transaction_with_behavior()` run
prepare before returning `WriterTransaction`; beginning can fail with the original prepare
error cause. Both begin methods return `anyhow::Result<WriterTransaction>`. It dereferences to the ordinary `rusqlite::Transaction`, so free
helpers taking `&Transaction` keep working. Explicit `commit()` runs the finalizer and
returns `anyhow::Result<()>`, preserving the original SQLite error for downcasting. Drop
and `rollback()` roll back without finalizing. Nested savepoints finalize only at the
outer commit. Mutable transaction dereference and commit-on-drop are not exposed.

Commit observers remain post-commit. A finalizer's uncommitted changes cannot publish a
successful output event; if SQLite itself rejects commit after finalization, both source
and output roll back. Lent observers still run when the writer guard returns. Keep exactly
one IVM Publisher on the same Store, and subscribe before authoritative read/recheck.

Raw `Connection` access, `WriterGuard.connection`, direct `Transaction::new`,
`unchecked_transaction`, SQL `BEGIN`/`COMMIT`, autocommit writes and work before installation
bypass this boundary. They are deliberately available for low-level operations. The
source adapter must capture/fence those paths independently; this API cannot reconstruct
missing old values or infer admission/authority from the latest store index. A raw bypass
is tested to leave captured input unprocessed rather than silently invoke the finalizer.
A callback that issues its own commit violates the contract; a subsequent error cannot
undo a commit already made through raw SQL.

A production adapter still needs complete extraction and mutation coverage, canonical
rank and per-kind repair eligibility, signer/authority dependencies, local observation and
deadline inputs, bounded initial installation, restore/checkpoint handling, rollback and
same-snapshot read certification. Neither callback installation nor a maximum applied
index makes a projection Ready. Existing runtimes install no callback by default.

# Read after a committed write

`ivm::after_write` adds an opt-in application API; it installs no routes, source adapters,
backfills or catch-up worker. Install the event feed explicitly and retain one Publisher.
`write` calls the existing admitted `Store::append_claim` and captures a `WriteReceipt`
containing database identity, source epoch, claim ID and admitted local index. If metadata
capture fails after the write, `CommittedWithoutReceipt` retains the committed claim and
reason; it must not be presented as rollback or cause a blind command retry. Production
runtimes retain their existing admission and idempotency policies.

Use `Target::Local` on the writing database. Database/epoch changes, a missing local claim
or a changed local position require explicit resync. Never reinterpret a replaced local
database as a remote target. On another machine choose `Target::Replicated` explicitly:
it resolves the content-addressed claim ID through the receiver's primary key. Its local
position, not the sender's position, is the threshold. An absent remote claim waits for
admission. A receipt is an untrusted processing requirement, never authorization or proof
that replication will deliver its claim. Validate peer/client scope separately.

`inspect` runs in the same short read snapshot as output. `wait` subscribes before that
snapshot, releases it before awaiting, and rechecks after committed notices or receiver
lag. `wait_subscribed` accepts an owned receiver so an independently owned publisher may
shut down. Both require a caller deadline and cancellation channel; cancellation closure,
timeout and publisher shutdown are explicit outcomes. Missing/fenced/incompatible views
remain pending; missing event schema or database errors propagate. No polling/replay or
timeout retry occurs. A provider identity change during a wait requires resync.

Ready requires the receipt's claim to be present, the certified source cut to cover its
receiving index, and `Views::readiness` to be Ready in that same snapshot. This currently
conservatively requires all admitted input to be processed, even unrelated input.
`MAX(applied_claim_index)`, semantic generation and invalidation sequences are not prefix
proofs. Runtime/adapters must publish source cuts only after every admitted input has been
processed or explicitly fenced. Unsupported same-index mutations retain their availability
fence. Historic installation and replacement-repair gates from IVM_EVENTS.md remain.

The output callback runs only after processing/readiness is certified. It must check
current authority, owner/incarnation and all declared dependencies in that same snapshot.
A diagnostic unavailable value is not successful action admission. Any later external
effect needs its own fresh fence; the returned value does not reserve authority.

Run the working example with normal Cargo configuration:

```sh
cargo run --locked -p smallclaims --example ivm_read_after_write
cargo test --locked -p smallclaims --test ivm_after_write
```

Restore/clone owners must rotate event database identity before exposure and certify their
source lifecycle separately. Disk reopen keeps receipts only while identity/epoch/source
remain compatible. This is not checkpoint or mixed-runtime adoption certification.
The next mission step implements bounded asynchronous catch-up behind explicit opt-in.

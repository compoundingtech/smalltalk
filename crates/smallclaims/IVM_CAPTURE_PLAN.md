# Native deferred capture plans

`Installer::capture_plan(connection, expected_source_position, table, ordered_pk)`
compiles literal-only SQL for an explicitly registered deferred source. It installs
nothing and creates no table, source, counter or queue. Existing Store users call
nothing. The source fingerprint must cover the complete native table manifest,
admission, extractor, authority and local-source dependencies.

The descriptor accepts ordinary main tables, at most 128 ordinary columns, and
the entire ordered primary key (1–16 columns). Names are validated and quoted;
source identities are escaped SQL literals capped at 4 KiB. Generated/virtual
tables and alternate unique keys are unsupported. Each append fragment is capped
at 256 KiB of compiled SQL.

The owner embeds `append_sql(Change::Insert/Delete/Replacement)` in an ordinary
main-schema AFTER trigger, with an explicit main-schema target. TEMP triggers
and execution of append fragments outside a trigger are unsupported. SQLite
requires unqualified trigger DML targets; their main binding follows that ordinary
trigger, while subqueries and scalar expressions explicitly target main. Setup
validates the main table columns and reads main source/deferred metadata even if
TEMP tables share their names. This binding does not validate physical schema
lifetime or prove absence of extra trigger work. Only actual PK cells are read:
no body expressions, normalization,
extraction, operator or validation callback runs. The key is JSON
`["table", [typed_primary_key_cells]]`; INTEGER, REAL and TEXT retain their JSON
types, and BLOB is `{"$blob":"UPPERCASE_HEX"}`. Null/nonfinite/oversized keys
fence the source while preserving the valid native mutation. Byte and null checks
short circuit before hex/JSON encoding; serialized keys are capped at 1024 bytes
and the complete journal reference at the configured limit (at most 4096 bytes).

Each queued Mutation has exact old/new locators
`{"table":"literal","revision":N,"side":0_or_1}`. Same-PK replacement has both
sides; insert has new, delete has old. Rekey must append Delete, retain its old
image immediately, then append Insert and retain its new image immediately.
Using Replacement for a changed binary value or SQLite type fences rather than dropping
the old dependency. `revision_sql()` and `available_sql()` are read-only scalar
expressions for the owner's native immutable-image inserts. They must execute
after EACH append, in the SAME outer source transaction. Missing images refuse
reactor preparation; mutable current rows never substitute for retained versions.

Appending uses the existing Installer source revision and journal, its protected
capturing flag, configured queue counters and active installation backlog. Source
fingerprint, epoch and exact capture limits must match the compiled plan. Queue
exhaustion or incompatible metadata fences roots/jobs; subsequent valid source
writes remain admitted but cannot claim Ready. The source revision saturates at
i64::MAX and fences there. `gap_sql()` supplies a fence fragment for an ordinary
main-schema trigger; outside a trigger, use `gap_main_sql()` in the same source
transaction so TEMP metadata cannot redirect the fence. Neither clears a fence
or sets Ready. Both carry the same fixed reason; this does not bound work on
unvalidated or replaced metadata.
SQLite/storage failures propagate and require rollback of the source transaction.

This is not a native source certificate. Owners must supply complete OLD/NEW
coverage (including implicit OR REPLACE deletions, repair, rank, signatures and
local producers), managed/raw scope detection, native-image quotas/retention,
and DDL/restore lifecycle fencing. Dropped Installer metadata or same-identity
source recreation is unsupported; the owner must refuse or fence it before any
write. AFTER-trigger images are not a substitute for BEFORE images on alias or
conflicting replacements. Never mix Rust capture_deferred and a trigger append
for the same mutation. Immutable cells remain source-owned until every active
namespace and install job passes their reference prefix.

The reactor uses the existing prepare/owned-reduce/publish APIs and one shared
Installer/Views/Publisher. Source availability and applied-prefix checks fence
reads until complete publication. No production adapter, migration, transport,
authority coverage or writer-time acceptance follows from compiling this plan.

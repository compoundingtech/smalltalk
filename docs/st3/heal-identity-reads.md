# Heal claim identity reads

Heal range comparison hashes writer, batch sequence and claim ID. It excludes a claim when
any retained replica record for that ID is repaired. A valid duplicate does not undo that
exclusion. Claims without replica records remain included.

`claims_batch_claim_id(batch_id,id)` covers the claim join and identity read.
`replica_records_repaired_claim(claim_id) WHERE state='repaired'` covers the exclusion check.
The range query explicitly uses both indexes, so it does not fetch claim bodies or retained
raw replica records. Subject narrowing still fetches the selected claims' subjects, as before.
Writer/sequence/claim ordering, range counts, digest domains and envelope selection stay the
same. SQLite maintains both indexes when claims or repair states change, including rollback.

Existing schema-17 stores build the two indexes on their first open with this change. That
construction scans the retained claims and replica records and delays store opening; its
duration on a populated fleet database has not been measured. Later opens reuse the indexes.
Accepted writes also maintain the indexes. No source rows, checkpoint rules or replication
wire fields change, and no replay is introduced. Reinstalling an older schema-17 binary leaves
the extra indexes in place; the retained pre-change query returns the same answers.

The isolated controls compare the retained query with the new identities and range digests,
including duplicate writer/sequence batches, repaired duplicates, missing records, state/ID
updates, deletes and rollback. They also check a real store reopening without the indexes.
Query plans must show covering identity reads. With 256 selected claims, increasing unrelated
repaired records from 512 to 8192 and payload bytes from 32 to 16384 must leave range VM work
unchanged. This does not make the full range comparison constant cost: it still reads all
projected identities and orders them. Production latency, first-open index construction and
concurrent API fairness need separate qualification.

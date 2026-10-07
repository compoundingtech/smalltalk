# Collection source mutation capture

`store::collection_ivm` stages complete old/new SQL rows for an explicitly selected source
schema. It is inactive by default and installs only through an explicit writer transaction.
It owns no Views registry, serving root, source cut, readiness or backfill protocol. The
shared runtime must feed these mutations to the existing foundation Installer and affected
key operators in the same source transaction before acknowledging capture.

Descriptors must cover every source column and the complete primary key. Inserts, updates,
key reassignment and deletes preserve old/new owner and dependency fields. Blobs retain exact
bytes through a tagged hex encoding. A key change produces retraction then insertion. Failed
or ignored inserts create no admitted mutation. INSERT OR REPLACE and UPDATE OR REPLACE
preserve overwritten primary-key rows with either recursive-trigger mode. INTEGER rowid
aliases, generated columns, alternate unique constraints, expression keys and virtual tables
require another explicit extractor and are rejected here. Exact table descriptors and source
schema SQL are persisted; changed source sets/schema under the same fingerprint stay fenced.

Capture has a shared limit of 256 rows and 1 MiB, with 64 KiB per replacement payload. Indexed
pages contain at most 128 captured statements. A quota/payload gap preserves valid source
writes, stops dispatch and remains unavailable across reopen. A gap cannot be cleared by
acknowledging rows. Populated installation, changed identity and omitted columns are fenced
or rejected; registration is not discovery of source coverage. These limits bound retained
capture, not SQLite expression execution time or the source application's own input size.

The transaction-start and precommit hooks are foundation-owned. Before any source operation,
the start hook must reject previously committed/unconsumed capture as coverage loss and
establish a managed scope. At precommit, bounded exact replacements enter Installer::record
and canonical/local affected-key dispatch, then acknowledgment. A callback that runs only at
precommit cannot distinguish earlier raw/autocommit capture from this source transaction.
Do not record a previously committed mutation as if it belonged to the current transaction.

Actual st3 registration is still unqualified. Canonical admission, signatures, accepted
repair, rank reassignment, local actor, person, incarnation and deadline closure, raw connection
bypasses, schema/restore/checkpoint changes and namespace/root-to-Views publication must be
proved by their adapter. Same-index uncovered work must produce committed unavailability
through the shared Publisher. The staging queue alone does not fence the Views registry or
activate a collection reader. Do not promote Ready, copy Installer status by revision, or
use a largest numeric index as a certificate.

Real Store controls cover source/Installer rollback in one outer transaction, blob retention,
ignored/replaced inserts in both recursive-trigger modes, key reassignment/removal, quota
admission preservation, and populated/schema/alternate-key refusal. These invented fixtures
prove capture mechanics, not completeness of a production collection family.

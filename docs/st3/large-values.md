# Large SQLite values

New storage work must avoid unbounded large inline SQLite objects. Design an
incremental, bounded physical representation or a reference with equivalent
logical semantics. This rule covers variable columns **and index keys**: claim
bodies and predecessors, raw signed envelopes, blobs, payloads, metadata,
identifiers, JSON, aggregate rows, and their derived values. An INTEGER-keyed row
can still contain a variable object. TEXT affinity, a route limit, or a bounded
number of rows does not establish a bound on its encoded bytes or statement cost.

There is no accepted numeric definition of “large” yet. This change introduces no
input limit, size threshold, runtime guard, schema, storage format, or rejection
of previously valid data. All valid current and admitted historical values remain
in scope. Existing producer-specific limits remain in effect and must not be
removed or generalized to other producers. Chunk size is a physical format
choice to qualify later, not a logical input cap.

## Inventory and transitional exceptions

[large-value-sites.json](large-value-sites.json) is the machine-readable source
inventory. Each `sites` entry names a SQL template, unresolved execution/bind
sink, or included source; its adjacent Rust `allow-for-now` comment names the
same stable ID, owning **producer module**, and a breakup/reference TODO.
IDs use `lv-h<12hex>` / `flow-h<12hex>`: the alphabetic hash tag distinguishes
source fingerprints from numbered decision citations. Full content fingerprints
remain separate and unchanged by that namespace.
`declarations` records every column and index expression found by the declared
lexical catalog, including types that look small. A template that cannot be
resolved stays explicitly unresolved. No catalog entry proves a value's size.
`flows` additionally binds the whole non-comment function source of direct
append, local sealing, replication admission, blob promotion, backup import and
physical/historical copy, restore and trim entry points. These paths can bypass
client extractor limits while still enforcing their own authority rules.

These markers record transitional debt. They neither grant permanent permission
nor transfer source ownership. The source module owns the TODO until its normal
maintainer changes the representation through review. A changed statement,
function, owner, TODO or dependency set requires an explicit inventory review;
removing a site requires removing its exception. There is no “accept all” or
regeneration command in the check. The first inventory deliberately keeps
conservative candidates instead of silently treating an ambiguous call as safe.

The inventory uses one complete entry per physical line (including nested column
metadata), with site records ordered by `(path, id)` and manual flows by
`(path, function)`. Declarations follow deterministic source-path/statement order,
with table, added-column and index groups within each template. Keep this layout
when reviewing changes so unrelated additions can merge as independent records.

Use the read-only maintenance helper after a source edit:

```sh
python3 scripts/propose-sqlite-large-values > /tmp/sqlite-value-proposal.json
```

It prints new, changed and removed IDs with exact fingerprints, proposed owner/TODO
metadata, declaration changes and stale manual flows. An existing adjacent marker
is the only anchor used to pair a changed ID; without it a replacement stays an
addition plus removal, not a guessed match. The output is **a proposal**, not an
accepted inventory. Review the actual producer and value flow, owner and concrete
TODO, then manually reconcile the selected records, adjacent comments and column
metadata. Run the guard afterward. The helper edits no file, cannot make a failing
guard pass, and has no accept/write switch. Unchanged source records are not
rewritten merely because line numbers moved.

TODO codes mean the following concrete work:

| TODO | Required follow-up |
| --- | --- |
| `chunk-or-reference-variable-columns` | Identify all variable columns and index keys; choose bounded physical units or references and preserve query semantics. |
| `prepare-body-and-predecessor-values` | Prepare body/dependency data outside the writer, then publish a validated short atomic result; preserve canonical values and authority. |
| `chunk-or-reference-exact-wire-values` | Preserve exact signed/raw bytes, identities and historical decoding while changing physical value storage. |
| `stage-generation-and-keyed-reclamation` | Prepare exact generations/closure outside the writer, validate the visibility switch, then reclaim by certified dependency keys. |
| `trace-statement-and-value-representation` | Resolve the dynamic statement/bind/import source and connect its actual values to a reviewed representation. |
| `bound-or-reference-values-and-reclamation` | Establish the producer's complete value/key and cleanup costs, then remove the unbounded inline dependency. |

A new exception's linked review must explain its actual producer, value
columns/keys, reason for remaining inline, and next representation or cleanup
step. Record the responsible maintainer and that explanation in the review,
linked to the inventory ID. The marker's `owner` is a deterministic source-module
code and its `TODO` is a deterministic work-category code; the guard requires
those codes to match the source and does not accept free-form reviewer names or
rationale in their place. It checks neither the linked review's existence nor
its approval. Owner/TODO strings alone are not cost, correctness, or storage
approval; do not replace a TODO with a permanent exemption.

## What the static guard checks

Run these lightweight source controls; neither opens SQLite or compiles Rust:

```sh
python3 scripts/check-sqlite-large-values-test
python3 scripts/check-sqlite-large-values
```

The same commands run through `scripts/ci-genie-freshness` in ordinary hosted CI
and the existing Nix check. The pass line prints guard elapsed seconds from its
actual source scan. In the initial authorized static check, the guard subprocess
took 11.441 seconds wall time and the 19 original controls took 0.061 seconds
including Python startup; these are source-check observations on that executor,
not native storage measurements or a hosted-runtime guarantee. The proposal helper
is not added to CI. Retain hosted guard elapsed separately when it runs.
The guard scans Rust files under `crates/**/src/**`.
It detects literal INSERT, REPLACE, UPDATE, DELETE, CREATE, ALTER and DROP
statement templates, including those inside triggers. It separately inventories
ambiguous `.execute`, `.execute_batch`, `.prepare`, `.prepare_cached` and
`.query_row` arguments in crates declaring rusqlite, libsqlite3-sys or sqlx.
Prepared-statement bind calls stay unresolved even when a parameter is a string
beginning with SELECT. New SQLite dependency crates change the coverage inventory.
`include!` is inventoried everywhere; `include_str!` is inventoried in those
SQLite crates. A new standalone SQL file under `crates` fails until its coverage
is added. New covered sites without matching comments fail, as do removed,
duplicate, malformed, orphaned or changed exceptions and altered column catalogs.
The guard compares exact template bytes and comment-free dynamic argument tokens;
it does not infer receiver types or evaluate Rust/SQL.

Explicit inline `#[cfg(test)]` items are excluded. Arbitrary cfg expressions are
not evaluated; separate test-only source files and diagnostic strings may be
conservatively included. This makes the manifest a candidate source inventory,
not a count of production writers. The column parser handles literal CREATE
TABLE/INDEX lists and ALTER TABLE ADD COLUMN declarations, not an evaluated SQL grammar: generated table/column names,
other ALTER-derived shapes, implicit/automatic indexes and runtime schema introspection
need source review. A literal declaration is not proof of all installed schemas.

**Blind spots remain:** macro expansion, SQL assembled from fragments, generated
OUT_DIR files, external Runtime implementations, raw FFI/other database APIs,
indirect sinks outside the declared dependency crates, and value growth through
bindings, serialization, JSON expansion, aggregation or indexing. A read-looking
template can be changed dynamically. A source reviewer must extend coverage when
introducing one of these paths. A green guard is source-site coverage within this
declared boundary, never complete type/dataflow analysis, an all-mutator evidence
certificate, a size proof, or a latency result. The manual flow inventory is finite;
new delegation paths need human review even if a lower covered sink already exists.

## Value and producer review checklist

| Value family | Producers and paths to follow |
| --- | --- |
| Claim body, predecessors, actor/subject/kind/operation/request IDs | Direct Core insertion and append; application/native admission; local document writes; registry-valid generic fields; SQL repair and historical restore. |
| Raw envelope/header/payload and dependency lists | Local signed sealing, current and legacy replication decoding, backup pages and offline imports. Preserve raw identity independently of derived JSON. |
| Blob bytes, staged values and promotions | Direct blob put, staged-to-durable promotion, legacy separation, backup/copy/reopen. A digest key does not bound the referred value. |
| Tombstone and checkpoint metadata | Manifest adoption, exact dependency closure, capture, proof staging, trim rollback/redelete and generation publication. |
| Projection/cache/document/response values | Actual parameter bindings, local observation and idempotency writers, runtime finalizers and repair paths; separate logical authority from local storage. |
| Variable keys and aggregates | Every declared index/uniqueness/foreign-key expression, dynamically generated DDL, JSON/string aggregation and serialized metadata; review both insert and delete costs. |

HTTP extraction and exchange/decompression limits are specific to their entry
points. Local append/sealing, direct native receive, backup and physical clone
have different contracts. JSON escaping, base64, CBOR and signed wrappers change
encoded size. SQLite's compiled/runtime limits constrain rows as well as values;
they are not a demonstrated reachable application maximum. The inventory makes no
claim about the largest currently stored value or actual native build limits.

## Cleanup and migration plan

Immediate logical hide or inactive generation publication must preserve the whole
logical source and all authority/certification predicates. Prepare exact contents,
manifest and dependency closure away from the live writer. Publish only a short,
validated atomic visibility change. Old readers must continue to see their pinned
cut; partly verified certificates, admission, inventory, capture, replay, digest,
repair, export and resume consumers must all use the same visibility semantics.
Inactive physical writes may be logically neutral but still require automatic
source-owned mutation evidence. An ordinary legacy/raw write to a masked key must
not evade evidence using a caller-supplied housekeeping flag.

Physical cleanup comes later, durably and incrementally, after reader/proof/page
pins close. Expiring a cursor rejects new accesses using the existing typed error;
it does not release a page already in flight. That page must finish or return its
typed error and close its owner before reclamation. Bounded deletion by row count
alone is insufficient: one existing cell's overflow chain is released inside one
DELETE or size-changing UPDATE. Replacing its body with a pointer or NULL also
frees that old chain. Triggers, cascades, metadata/index keys, digest maintenance,
runtime finalizers, commit/fsync, rollback and actual connection return are part
of the cost. Logical row cleanup, WAL checkpointing and physical file space
reclamation are distinct obligations. No smaller cap, longer timeout, disabled
functionality or indefinite legacy fallback completes this plan.

A forward conversion must cover the largest valid current and admitted historical
cells, including the old representation's release. Compare stopped in-place
conversion against isolated replacement/retired backing using the **actual all-DB-
opener** contract, disk budgets, crash states and write-preserving recovery. An
old snapshot restore can lose acknowledged later writes and is not a default
rollback plan. Preserve exact raw bytes, canonical JSON/merge semantics, indexes,
streaming reads, native authority, repair exclusion, digests, cursor identity,
copy/backup/open paths and fail-closed recovery before activation. Missing backing
must return an explicit open/access error; it must never silently select an older
root. External data, if selected, must become durable before its directory and
main-database pointer publication. There is no migration, stop window or execution
permission in this rule change.

## Accepted source directions and remaining decisions

The source-only directions below authorize design/control preparation. They do
not select real DDL, fields, indexes, thresholds, wire changes, a schema version,
an executor, a migration or an operational window:

- **S1:** inactive SQL generations (D) with certified logical hide and dependency-key
  reclamation (C); evaluate one forward versioned conversion, staged readers and
  one old-binary fence. No live-writer primary-key rebuild.
- **S2:** compare a hybrid first: small inline values preserve SQL/index semantics;
  large values use SQLite chunk rows with exact extracted/index metadata. External
  extents are for an evidenced residual. No threshold or chunk size is selected;
  a hybrid does not remove any consumer/JSON/index/trigger/authority obligation.
- **S3:** compare a stopped in-place converter with isolated replacement/retired
  backing, first obtaining held operational pricing, all-opener and write-preserving
  recovery facts. No tool, copy, stop or window is authorized.
- **C1:** design additive negotiated manifest identity in responses and
  continuations, preserving existing page-cursor-expired 410 behavior. Exact new
  semantics still require contract review; this change alters no wire field.

Real representation/conversion decisions require separate storage/contract review
and the required human decision before implementation. Native cost controls,
large-cell fixtures, long-reader/WAL checks and crash/reopen tests remain future
qualification work. This source guard supplies none of those results.

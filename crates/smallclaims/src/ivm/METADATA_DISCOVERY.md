# Canonical prepared metadata discovery: first factoring slice

PR2143 landed at ce7bdadaf849a0450021cdde93ebf4aec3573a28, tree
48d16d01877ead7ab39d2bd68821f69b45affa8a, at 2026-10-10T07:15:32Z.
This follow-up is source preparation against that landed/current-main pin.
Nothing here activates a source or proves metadata work at scale.

`PreparedPage::capture_table_image` returns a private, one-call `TableImage`
containing the same requested metadata that was previously staged as a local
map. It executes the existing shape query and complete main foreign-key walk,
with identical discovery SQL, error texts, caps and refusal order. Unrelated
tables participate in the complete inbound-FK proof and existing FK/inventory
bounds; their own namespace/PK/generated-column/trigger eligibility is not
validated as an output shape.

Argument/identifier/request-union validation stays before discovery. The
final schema cookie equality stays before `self.tables.extend`, and the
publication schema check stays before output writes. No image is stored on
Installer, returned to a consumer or reused between calls or cuts. There is
no new public API, owner, lease, retention policy, proof capability, cache,
schema, byte cap, DDL interception, background producer or runtime wiring.
Factoring changes no query count or advertised cost.

All 26 earlier ledger/metadata controls and the strict full-page accounting
assertions remain. The new unrelated generated-column/trigger control checks
wrapper/group parity, mixed-case composite inbound refusal, preserved earlier
metadata and no partial sibling acceptance against the actual helper.

The new real-Store characterization control
`canonical_metadata_inventory_growth_reports_actual_page_work` prepares
separate file-backed fixtures with 0, 1, 8, 32 and 128 unrelated main tables,
created before the fixture's schema-lifetime stamp. Each fixture applies two
16-input pages with the same subject widths and body content across inventory
sizes. It checks actual copied input rows/bytes match for each page ordinal,
and output parity after publication. Each capture still performs canonical
discovery; there is no cold/warm shortcut.

It prints capture and publication SQL/VM/fullscan/sort/autoindex counters and
their whole-page total, including the physical reader and writer returns,
before verification queries. A separately charged, fixture-only schema probe
prints inventory rows and the actual name/declaration bytes copied by that
diagnostic probe. Those bytes are neither the production metadata helper's
copy bytes nor SQLite internal scan/allocation bytes. Copied input bytes are
actual `OwnedPage` input bytes. This instrumentation is explicitly partial;
it does not manufacture a complete metadata-byte work certificate.

The growth control is characterization, not an alternate bound gate. Its
printed `within_original_statement_ceiling=false` is adverse evidence even
when its correctness/measurement assertions pass. The original full-page
control retains positive statements, <=128 statements, <=50000 VM and zero
autoindex, with the original interval. A characterization pass cannot waive
that prerequisite or qualify expanded inventories. No count, saving, slope
or successful execution is claimed from this source.

An exact package/binary/name nextest override makes successful growth output visible; it changes only success-output, preserving the existing strict override and all inherited selection/retry/thread/budget settings. CI custody is message/2aaaf079d476cd45.

All new controls are UNRUN. Rustfmt/diff/static source checks do not establish
Rust compilation, SQLite behavior or numerical work. A later normal hosted
assignment must positively select each new name and retain its printed tuple;
the no-test-support twin fails rather than omitting the measurement. Keep the
landed cc387 hosted tuple (128/6344/642/1/0), older 129/135/273 failures and any
future growth outcomes attributed to their actual source and composition.

Bring measured growth to the sole curator before considering any proof
capability. Statement versus VM cost is a question for evidence and review,
not permission to relax Nathan's invariant. Native/canonical/lifetime,
schema ownership, work at scale, fatal writer/reader cleanup and production
adoption obligations remain separate and open.

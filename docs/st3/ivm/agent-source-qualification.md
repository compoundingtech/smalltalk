# Isolated agent-source applicability

`scripts/qualify-agent-source` runs one actual `ivm_agent_qualify` process against
an existing consistent scratch copy. It starts no daemon, reconciler, harness or
socket. It never opens the live database. Ops owns the backup and execution; the
vehicle does not authorize live source activation.

Use the supported SQLite read-only online backup route (Python `sqlite3`, backup
pages=512, sleep=0.02) into a new private scratch directory. Retain its completion,
original receiver/config identity, source database/version, start/end timestamps,
and consistency provenance in a private receipt. Do not use a file copy of a live
WAL database, an offline exporter against the live store, or a guessed receiver.
The selected hosts are recorded in the private Ops instruction; hosts with disk pressure are excluded.
This source qualification is one attempt per selected backup, not repeated scans.

Build the exact reviewed vehicle head through the host's ordinary Cargo wrapper:

```sh
env -u LD_PRELOAD CARGO_BUILD_JOBS=6 RUST_TEST_THREADS=4 \
  nix develop -c cargo build -p st3 --example ivm_agent_qualify
sha256sum target/debug/examples/ivm_agent_qualify
git rev-parse HEAD HEAD^{tree}
```

The example must come from this vehicle commit, based on PR1865's source. It adds
only read-only source inspection plus the standalone runner. Operator, capture,
installer, fingerprints, readiness and limits are unchanged. Record the resulting
binary hash/head/tree; the script checks the binary hash and records the pins.
Linux execution is tested. macOS execution and populated-host resource fit remain
unqualified until Ops runs the same supported vehicle there.

Create `.ivm-qualification-scratch` in the private backup directory, containing
its provenance. The directory must contain `backup.sqlite`; symlinks are refused.
The backup receipt JSON must include these fields, alongside the retained backup
provenance (the receiver hash is SHA256 of its exact UTF-8 config value):

```json
{"method":"sqlite-online-backup","completed":true,"original_receiver_sha256":"<64 hex>"}
```

Run once, using absolute paths and the original receiver from the same backup:

```sh
python3 scripts/qualify-agent-source \
  --binary "$PWD/target/debug/examples/ivm_agent_qualify" \
  --binary-sha256 "$QUALIFIER_SHA256" \
  --source-head "$QUALIFIER_HEAD" --source-tree "$QUALIFIER_TREE" \
  --scratch "$PRIVATE_SCRATCH" --original-receiver "$ORIGINAL_RECEIVER" \
  --backup-receipt "$PRIVATE_BACKUP_RECEIPT"
```

The supervisor uses nice10, a 900-second wall deadline including Store startup,
900 CPU seconds, 4GiB address space, 32GiB per file, at most128 scratch files and
16MiB output per stream. It checks a 64GiB scratch threshold every100ms; this is
an abort threshold, not a filesystem quota. Temporary SQLite files stay in that
scratch directory. On limits/deadline it terminates the owned process group,
then kills after2 seconds. Keep at least80GiB free before starting; do not prune
other hosts' state to make this run fit. Backup time is additional to900 seconds.

The actual source retains its limits: 1,000,000 extracted rows,64KiB raw cells
and encoded physical rows,128 rows/1MiB pages,16,384 rows/16MiB pending journal,
1-second callbacks and1-hour installer lifetime. No cap is increased. The runner
calls the normal bounded pump; zero-progress pages wait100ms. Unsupported,
raw-gap, authority, local, deadline, source, producer and closure refusals retain
their normal guards. The deadline cannot convert an old root into readiness.

TEMP triggers maintain sixteen tiny per-table cardinality gauges as the operator
already writes its physical shadow. They do not rescan native inputs or alter
the main schema fingerprint. Final counts belong to the genuine newly published
namespace at a complete current cut; the live boundary/source/cut/namespace are
rechecked after sampling. Counted clock and producer replacements are scratch
inputs, so the final total can exceed the baseline extraction count. Both total
and observed maximum encoded row must fit the original limits. `max_page_us`
comes from the real Installer and `max_pump_us` measures the actual pump; these
measurements include the TEMP gauge overhead, not a deployed callback profile.

Exit0 means `QUALIFIED_SCRATCH_ONLY`; exit2 means unavailable/refused, and124
means supervisor timeout. Input/provenance/binary mismatch refuses before opening
the source. `.qualification-started` prevents another attempt after interruption.
`qualification.json` records exact pins, limits, cut, namespace, job progress,
sixteen counts and measurements on success. Private bounded stdout/stderr stay
beside it; publish only metadata receipts, never source contents.

This is applicability evidence for that backup. The child uses its own fresh
producer process, not the live daemon's heartbeats, reports or acknowledgements.
Live producer health, current post-backup source fit, resulting-head required
checks, deployed Linux/macOS CPU/callback profiles and physical device behavior
remain separate gates. Keep the flag off on refusal, timeout or unknown evidence.

After retaining the private result/provenance and publishing the metadata receipt,
Ops removes only the exact marked scratch directory it created. The vehicle leaves
it intact for diagnosis; it never uninstalls production triggers or deletes live
state. No automatic retries, restore surgery or extra collector are part of it.

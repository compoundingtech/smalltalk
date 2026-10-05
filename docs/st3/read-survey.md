# Read latency survey

`scripts/read-survey` inventories local GET routes and installed CLI help paths, then probes
an isolated copy of a store. It is a survey tool, not a p99 acceptance gate. Its JSON records
route templates, HTTP status or process exit counts, descriptive percentiles, and proposed
budgets. It does not retain resolved graph IDs, response bodies, or subprocess output.

Make a consistent SQLite backup first, using the SQLite backup API with a pinned read
transaction, and keep the original immutable. Make a fresh writable clone for each cohort.
Copying a live database and its WAL separately is not a consistent snapshot. Record the
snapshot hash, schema, graph counts, source revision, binary hashes, and host load privately.

Build and run the API harness on the writable clone:

```sh
cargo build --release --locked -p st3 --example read_survey_server --bin st3
env -u ST_AGENT -u ST3_PROFILE_DIR target/release/examples/read_survey_server /tmp/survey-copy
```

The harness starts the daemon's asynchronous diagnostic reporter. It starts no reconciler,
driver, peer, or external transport. It uses a fresh invented local identity and has no live
PTY or native session files. `Store::open` can migrate the copy; never pass a production state
directory. Store responses can contain private data, so keep the scratch directory private.

In another shell:

```sh
scripts/read-survey --binary target/release/st3 --socket /tmp/survey-copy/st3.sock \
  --database /tmp/survey-copy/claims.sqlite3 --samples 5 --cli-reads --protocol-reads \
  --invalidation-reads --output /tmp/survey-results.json
```

CLI reads use an isolated XDG config/state directory and the explicitly supplied endpoint.
Read receipts, invented agent-label edits, and invented document publications mutate only
the clone. Custom-basis probes expect the public custom-review example to be registered and
an invented `custom/garden/review/v1/survey` subject to exist; otherwise they remain fixture
gaps. Additional fixtures are needed for native drivers, PTYs, authenticated peer transport,
blobs, glass details, owned sets, and planning decisions.

Treat a 2xx response or successful process exit as the start of semantic validation, not
proof that the expected content was read. An empty collection, cached snapshot, error,
redirect, or help response cannot substitute for a successful populated fixture. Review
individual status counts and fixture labels, and retain unsuccessful probes. Backup and
stream probes measure admission or first byte; transfer completion and stream updates need
separate measurements. A long poll's intentional wait is separate from computation.

Use many more observations and a documented CPU shape for acceptance. Test cold reads,
warm reads, affected and unrelated writes, canonical reorder, replication, rollback,
reopen, checkpoint changes, and concurrent writes. Pair latency limits with SQLite work
counts and incremental-versus-full recomputation oracles. Run profiling in a separate
cohort; profiled timings cannot replace unprofiled results.

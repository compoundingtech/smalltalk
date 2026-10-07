# Claim backups

`st backup create FILE` saves one live daemon snapshot as a private file. It reads signed sync
envelopes on a pooled read worker in pages of 512, then downloads the completed archive. Writes
and other requests continue while it reads. A slow download does not keep the database snapshot
open. The daemon needs temporary disk space for the archive; the destination is published only
once its checksum and end record verify. Existing backup files are never overwritten. If
replication has admitted claims whose projections are still pending, or the graph is stale,
export refuses before writing an archive; retry after the graph has recovered. A missing writer
predecessor also prevents publication until that history arrives.

```sh
st backup create before-upgrade.jsonl
st backup restore before-upgrade.jsonl --database /var/lib/example/recovery/claims.sqlite3
```

Restore runs offline, builds a temporary database at the current schema, and publishes it only
once admission, projection and digest checks pass. It refuses a destination with existing claims.
Without `--database`, restore selects the configured state's `claims.sqlite3`. Stop the node before
restoring there. Neither command installs services or starts a daemon.

For rehearsals, make a consistent SQLite copy first, then export that copy:

```sh
st backup create rehearsal.jsonl --database /var/lib/example/copy/claims.sqlite3
st backup restore rehearsal.jsonl --database /var/lib/example/restored/claims.sqlite3 --json
```

The offline exporter migrates and finishes upgrade recovery on its input, so use a copy. Its header identifies the exporting
build and the schema of that migrated snapshot. A pinned old daemon writes the compatibility test
fixture; the current exporter archives a copy of that graph and the current restorer verifies
that its envelope inventory, claim sources and all graph table digests match.

## Rehearse a restore

Use a healthy current daemon and a separate empty directory. These commands neither start a
daemon nor connect the recovered state to your live fleet:

```sh
(
  set -eu
  umask 077
  st_recovery_root=$(mktemp -d "${TMPDIR:-/tmp}/st-recovery.XXXXXX")
  st backup create "$st_recovery_root/claims.jsonl"
  mkdir -p "$st_recovery_root/restored"
  st backup restore "$st_recovery_root/claims.jsonl" \
    --database "$st_recovery_root/restored/claims.sqlite3" --json \
    > "$st_recovery_root/restore-report.json"
  printf 'Private rehearsal files: %s\n' "$st_recovery_root"
  cat "$st_recovery_root/restore-report.json"
)
```

Always pass `--database` for this rehearsal. Without it, restore selects the configured live
state directory; that default is not the separate-state procedure. Stop here if export or
restore fails. The commands run in a subshell so the umask does not change your interactive shell.

Keep the report private. Check the returned `writer`, `envelopes`, `source_graph_digest`,
`graph_digest`, `log_digest` and `projections_match` against the format rules below. A different projection
schema can legitimately rebuild different tables; equal envelope history is mandatory, and a
successful command alone does not establish identical projections across versions.

To run this recovered node later, configure a **new** state directory, its returned `writer`
as `node`, and distinct API and gateway sockets. Use the current verified binary and fresh local
keys. Never point the live service at this directory as part of the rehearsal, copy the original
writer's keys into it or start two nodes with the same writer identity. Follow [Starting a
recovered node](#starting-a-recovered-node) for fleet joining. Keep the original member intact
until a separately planned recovery accounts for writes since the snapshot.

## Format and verification

The archive is UTF-8 JSON Lines, format `st3.claim-backup`, version `1`. Each record is limited
to 128 MiB; export refuses a larger record before publishing a file:

- `header`: format version, exporting build, database schema and claim registry digest, fleet ID
  and public anchor, complete admitted envelope-log digest, graph digest, and each shared table's digest and row count.
- `envelope`: the unchanged `ReplicaEnvelope` sync wire record, including writer, sequence,
  previous batch hash, payload hash, accept time, base64 CBOR claims and document bytes, and the
  writer's public key and signature when present. Envelopes sort by writer, sequence, hash;
  projections replay claims in their normal canonical total order.
- `signatures`: sync signature records, including additional signatures held for those envelopes.
- `checkpoint`: the sync manifest of the newest applied or in-progress trim, including older
  tombstones. Restore initializes empty checkpoint state through the normal certificate checks,
  even if the source knew a newer certificate it had not applied yet.
- `end`: envelope count and SHA-256 of all preceding bytes, including line endings.

Restore verifies signatures, envelope and claim hashes through normal admission, checks writer
chains, reapplies replicated repairs, and replays projections with the current code. Pre-membership envelopes retain their
unsigned legacy wire representation. As in normal replication, later membership changes do not
revoke envelopes already admitted. Missing or undecodable envelopes fail the restore. Unknown
or quarantined claim records retain the normal admission behavior; the admitted source digest
must still match exactly.

The complete envelope-log digest always has to match, including certified tombstone identities.
With the same registry fingerprint, the claim source digest and count also have to match. The
fingerprint covers the known claim kinds and shared projection layout, as it does for sync.
With the same schema and fingerprint, all table digests and the graph digest also have to match.
An older schema or registry can produce
new projections after an upgrade, including previously unknown claims: JSON output reports the source and rebuilt graph digests,
`projections_match`, and rebuilt table digests so that this difference is visible. Envelope payloads
remain unchanged. Keep old claim readers in the registry when adding new kinds; SQL migrations
are not a backup compatibility mechanism.

## Starting a recovered node

Restore returns a fresh writer named `restored-...`. Set `node` in the recovered node's config to
that exact value before starting it. The database refuses another writer at startup. Use a fresh
state directory, generate new local keys, and join the fleet as that new member. This avoids ever
signing a sequence that the original writer already used, including writes after the backup.

The archive contains durable shared claims, their referenced blobs, public signature material and
checkpoint tombstones. It does not contain private node keys, invite secrets, device capabilities,
driver state, active terminals, local observations, lease renewals, transcripts, staged blobs,
configuration, or other local caches. Protect the file like the original graph: claims and document
bytes can contain private information. A restored standalone graph can be inspected immediately;
running seats and fleet transport require fresh local setup.

The format deliberately uses sync records. A receive-only continuous backup peer is future work.

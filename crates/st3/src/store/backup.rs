//! Backup reads pin a pooled reader and page the sync envelopes; restore uses normal admission.
use super::*;
use crate::backup::{Header, PAGE, Record, RestoreReport, TableDigest, write_record};
use crate::model::{ReplicaEnvelope, ReplicaEnvelopeId, ReplicaEnvelopeSignature};
use std::io::Write;
use std::sync::atomic::Ordering;

impl Store {
    pub fn backup_header(&self) -> Result<Header> {
        self.read_snapshot(|_| self.snapshot_backup_header())
    }

    fn snapshot_backup_header(&self) -> Result<Header> {
        let connection = self.readers.get();
        let digests = projection_digest::tables(&connection)?;
        let counts = connection
            .prepare(
                "SELECT table_name,row_count FROM projection_digest_state ORDER BY table_name",
            )?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(Header {
            format: crate::backup::FORMAT.into(),
            version: crate::backup::VERSION,
            source_build: st_drivers::version::machine_version(),
            source_schema: connection.query_row("PRAGMA user_version", [], |r| r.get(0))?,
            registry_digest: self.runtime.schema_digest(),
            fleet_id: fleet_meta(&connection, "fleet_id")?,
            fleet_anchor: fleet_meta(&connection, "fleet_anchor_key")?,
            graph_digest: projection_digest::root(&digests),
            log_digest: self.backup_log_digest()?,
            tables: digests
                .into_iter()
                .map(|(name, digest)| {
                    let count = counts[&name];
                    (name, TableDigest { digest, count })
                })
                .collect(),
        })
    }

    fn backup_log_digest(&self) -> Result<String> {
        let connection = self.readers.get();
        let mut hash = Sha256::new();
        hash.update(INVENTORY_DIGEST_DOMAIN);
        let mut after = (String::new(), 0_u64, String::new());
        loop {
            // Both index seeks are bounded before the UNION sorts at most two pages.
            let identities = connection.prepare_cached(
                "WITH live AS (SELECT writer,sequence,envelope_hash FROM replica_envelopes
                   WHERE (writer,sequence,envelope_hash)>(?1,?2,?3) AND batch_id IS NOT NULL
                   ORDER BY writer,sequence,envelope_hash LIMIT ?4),
                 dropped AS (SELECT writer,sequence,envelope_hash FROM checkpoint_envelopes
                   WHERE (writer,sequence,envelope_hash)>(?1,?2,?3)
                   ORDER BY writer,sequence,envelope_hash LIMIT ?4)
                 SELECT * FROM live UNION SELECT * FROM dropped ORDER BY writer,sequence,envelope_hash LIMIT ?4"
            )?.query_map(params![after.0,after.1,after.2,PAGE], |r| Ok(ReplicaEnvelopeId {
                writer:r.get(0)?,sequence:r.get(1)?,hash:r.get(2)?,
            }))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if identities.is_empty() {
                break;
            }
            for identity in &identities {
                update_identity_digest(
                    &mut hash,
                    &identity.writer,
                    identity.sequence,
                    &identity.hash,
                );
            }
            let last = identities.last().unwrap();
            after = (last.writer.clone(), last.sequence, last.hash.clone());
        }
        Ok(hex::encode(hash.finalize()))
    }

    /// Run on a blocking read worker, never the writer queue. No page query scans the full log.
    pub fn write_backup(&self, output: &mut impl Write) -> Result<Header> {
        loop {
            self.seal_local_batches()?;
            // Capture before pinning: another sealer may advance this atomic after our reader
            // starts, but that commit would not be visible to the pinned snapshot.
            let seeded_through = self.seeded_batch_rowid.load(Ordering::Acquire);
            let result = self.read_snapshot(|_| {
                // A write can land between sealing and pinning the reader. Retry before emitting
                // anything if that snapshot has claims without their signed sync envelope.
                if max_batch_rowid(&self.readers.get())? > seeded_through {
                    return Ok(None);
                }
                // Admission and projection commit separately. Only a snapshot whose persisted
                // projection frontier covers its log can promise the same graph on restore.
                let health: Option<(String, u64)> = self.readers.get().query_row(
                    "SELECT status,last_good_store_index FROM projection_health WHERE aggregate='graph'",
                    [], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                let admitted: u64 = self.readers.get().query_row(
                    "SELECT value FROM meta WHERE key='replication_admitted_index'", [],
                    |row| row.get::<_, String>(0),
                )?.parse()?;
                anyhow::ensure!(
                    health.map_or(admitted == 0, |(status, through)| status == "healthy" && through >= admitted),
                    "backup requires a healthy, fully projected graph; retry after replication catches up"
                );
                let header = self.backup_header()?;
                let mut hash = Sha256::new();
                write_record(output, &mut hash, &Record::Header(header.clone()))?;
                let mut after = (String::new(), 0_u64, String::new());
                let mut count = 0;
                loop {
                    let connection = self.readers.get();
                    let identities = connection
                        .prepare_cached(
                            "SELECT writer,sequence,envelope_hash FROM replica_envelopes
                         WHERE (writer,sequence,envelope_hash)>(?1,?2,?3) AND batch_id IS NOT NULL
                         ORDER BY writer,sequence,envelope_hash LIMIT ?4",
                        )?
                        .query_map(params![after.0, after.1, after.2, PAGE], |r| {
                            Ok(ReplicaEnvelopeId {
                                writer: r.get(0)?,
                                sequence: r.get(1)?,
                                hash: r.get(2)?,
                            })
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    if identities.is_empty() {
                        break;
                    }
                    let last = identities.last().unwrap();
                    after = (last.writer.clone(), last.sequence, last.hash.clone());
                    drop(connection);
                    let signatures = self.replication_signatures_for(&identities)?;
                    for envelope in self.replica_envelopes(identities)? {
                        write_record(output, &mut hash, &Record::Envelope(envelope))?;
                        count += 1;
                    }
                    if !signatures.is_empty() {
                        write_record(output, &mut hash, &Record::Signatures(signatures))?;
                    }
                }
                // A trim records its complete certified tombstones before deleting any rows.
                // A snapshot between deletion chunks must include that manifest too.
                let checkpoint: Option<(String, u128)> = self
                    .readers
                    .get()
                    .query_row(
                        "SELECT id,cut_unix_ms FROM checkpoints
                     WHERE state IN ('trimming','trimmed') AND drop_digest IS NOT NULL
                     ORDER BY cut_unix_ms DESC LIMIT 1",
                        [],
                        |row| {
                            Ok((
                                row.get(0)?,
                                u128::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                            ))
                        },
                    )
                    .optional()?;
                if let Some((checkpoint, cut_unix_ms)) = checkpoint {
                    // The manifest is the sync representation, verified against signed certificate
                    // claims on restore. No SQL or local checkpoint bookkeeping enters the archive.
                    let mut after = None;
                    loop {
                        let page = self.manifest_page(
                            &CheckpointManifestRequest {
                                checkpoint: checkpoint.clone(),
                                cut_unix_ms,
                                after,
                            },
                            PAGE,
                        )?;
                        after = page.next.clone();
                        write_record(output, &mut hash, &Record::Checkpoint(page))?;
                        if after.is_none() {
                            break;
                        }
                    }
                }
                self.check_backup_chains()?;
                serde_json::to_writer(
                    &mut *output,
                    &Record::End {
                        envelopes: count,
                        sha256: hex::encode(hash.finalize()),
                    },
                )?;
                output.write_all(b"\n")?;
                Ok(Some(header))
            })?;
            if let Some(header) = result {
                return Ok(header);
            }
        }
    }

    pub(crate) fn prepare_backup_restore(&self, header: &Header, writer: &str) -> Result<()> {
        if let Some(fleet) = &header.fleet_id {
            self.bind_fleet(fleet)?;
        }
        if let Some(anchor) = &header.fleet_anchor {
            self.pin_fleet_anchor(anchor)?;
        }
        self.connection.write().execute(
            "INSERT INTO meta(key,value) VALUES('backup_restore_writer',?1)",
            [writer],
        )?;
        Ok(())
    }

    pub(crate) fn receive_backup_page(
        &self,
        header: &Header,
        envelopes: &[ReplicaEnvelope],
    ) -> Result<()> {
        let fleet = header.fleet_id.as_deref().unwrap_or("");
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        for envelope in envelopes {
            match (&envelope.member_key, &envelope.signature) {
                (Some(key), Some(signature)) => {
                    let message = crate::fleet::envelope_signature_message(
                        fleet,
                        &envelope.writer,
                        envelope.sequence,
                        &envelope.hash,
                    );
                    anyhow::ensure!(
                        crate::fleet::verify_signature(key, &message, signature),
                        "invalid backup envelope signature"
                    );
                    store_envelope_signature_tx(
                        &transaction,
                        fleet,
                        &envelope.writer,
                        envelope.sequence,
                        &envelope.hash,
                        key,
                        signature,
                        &now_ms().to_string(),
                    )?;
                }
                (None, None) => {} // Pre-membership sync envelopes have no signature.
                _ => anyhow::bail!("incomplete backup envelope signature"),
            }
            // Sync can hold several wire representations of one historical batch. Preserve
            // every envelope identity, in the same total order the exporter uses.
            let previous: Option<(u64, String)> = transaction
                .query_row(
                    "SELECT sequence,envelope_hash FROM replica_envelopes WHERE writer=?1
                 ORDER BY sequence DESC,envelope_hash DESC LIMIT 1",
                    [&envelope.writer],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((sequence, hash)) = previous {
                anyhow::ensure!(
                    (envelope.sequence, envelope.hash.as_str()) > (sequence, hash.as_str()),
                    "backup envelopes are duplicated or out of order"
                );
            }
            transaction.execute(
                "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,previous_hash,accepted_at_unix_ms,
                     payload,relay,receipt_state,received_at_unix_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,'backup','pending',?7)",
                params![envelope.writer, envelope.sequence, envelope.hash, envelope.previous_hash,
                    envelope.accepted_at_unix_ms.to_string(), envelope.payload, now_ms().to_string()]
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn receive_backup_signatures(
        &self,
        header: &Header,
        signatures: &[ReplicaEnvelopeSignature],
    ) -> Result<()> {
        let fleet = header.fleet_id.as_deref().unwrap_or("");
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        for signature in signatures {
            let message = crate::fleet::envelope_signature_message(
                fleet,
                &signature.writer,
                signature.sequence,
                &signature.hash,
            );
            anyhow::ensure!(
                crate::fleet::verify_signature(
                    &signature.member_key,
                    &message,
                    &signature.signature
                ),
                "invalid backup envelope signature"
            );
            store_envelope_signature_tx(
                &transaction,
                fleet,
                &signature.writer,
                signature.sequence,
                &signature.hash,
                &signature.member_key,
                &signature.signature,
                &now_ms().to_string(),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finish_backup_restore(
        &self,
        header: &Header,
        writer: &str,
        envelopes: u64,
        checkpoint: Option<&CheckpointManifest>,
    ) -> Result<RestoreReport> {
        self.validate_replication_backlog()?;
        self.apply_replication_repairs()?;
        // Reject any held or undecodable envelope rather than silently restoring a partial log.
        {
            let connection = self.readers.get();
            let unresolved: u64 = connection.query_row(
                "SELECT COUNT(*) FROM replica_envelopes WHERE batch_id IS NULL",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(
                unresolved == 0,
                "backup contains {unresolved} unadmitted envelopes"
            );
        }
        // Normal admission is final: later membership claims do not revoke history
        // admitted before their arrival. Preserve that history, including doctor residue.
        self.replay_replication_graph()?;
        if let Some(manifest) = checkpoint {
            self.restore_checkpoint_history(manifest)?;
            self.replay_replication_graph()?;
        }
        self.check_backup_chains()?;
        let actual = self.read_snapshot(|_| self.backup_header())?;
        anyhow::ensure!(
            actual.log_digest == header.log_digest,
            "restored envelope log digest differs from backup"
        );
        if actual.registry_digest == header.registry_digest {
            anyhow::ensure!(
                actual.tables.get("claim_sources") == header.tables.get("claim_sources"),
                "restored claim source digest/count differs from backup: expected {:?}, got {:?}",
                header.tables.get("claim_sources"),
                actual.tables.get("claim_sources")
            );
        }
        let projections_match =
            actual.graph_digest == header.graph_digest && actual.tables == header.tables;
        if actual.source_schema == header.source_schema
            && actual.registry_digest == header.registry_digest
        {
            anyhow::ensure!(
                projections_match,
                "restored graph digest differs from backup"
            );
        }
        Ok(RestoreReport {
            writer: writer.into(),
            envelopes,
            graph_digest: actual.graph_digest,
            source_graph_digest: header.graph_digest.clone(),
            log_digest: actual.log_digest,
            projections_match,
            tables: actual.tables,
        })
    }

    fn check_backup_chains(&self) -> Result<()> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT rowid,origin,replica_sequence,previous_hash FROM batches
             WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
        )?;
        let mut predecessor = connection.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM batches WHERE origin=?1 AND replica_sequence=?2 AND hash=?3)
                 OR EXISTS(SELECT 1 FROM checkpoint_envelopes WHERE writer=?1 AND sequence=?2)"
        )?;
        let mut after = 0_i64;
        loop {
            let page = statement
                .query_map(params![after, PAGE], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if page.is_empty() {
                break;
            }
            for (rowid, writer, sequence, link) in page {
                after = rowid;
                if let Some(link) = link {
                    anyhow::ensure!(sequence > 0, "backup writer chain has an invalid root");
                    // Retained predecessors must match their exact batch hash. A predecessor
                    // removed by a verified checkpoint has only its certified wire identity left.
                    let found: bool =
                        predecessor.query_row(params![writer, sequence - 1, link], |r| r.get(0))?;
                    anyhow::ensure!(
                        found,
                        "backup writer chain has a missing or broken predecessor; retry after replication catches up"
                    );
                } else {
                    anyhow::ensure!(
                        sequence <= 1,
                        "backup writer chain starts after a missing predecessor"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup;

    fn diagnostic(store: &Store, reason: &str) {
        store
            .append_claim(&ClaimInput {
                subject: format!("daemon/{}", store.origin()),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("backup-test")),
                    ("reason".into(), json!(reason)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    fn archive(store: &Store, path: &Path) -> Header {
        store
            .write_backup(&mut fs::File::create(path).unwrap())
            .unwrap()
    }

    #[test]
    fn signed_backup_restores_the_log_documents_and_graph_with_a_new_writer() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open(&root.path().join("source.db"), "alder").unwrap();
        let key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        source.bind_fleet("test-fleet").unwrap();
        source.pin_fleet_anchor(key.public()).unwrap();
        source.set_member_key(Some(key.clone())).unwrap();
        source
            .admit_fleet_anchor("test-fleet", key.public(), "listening")
            .unwrap();
        for index in 0..PAGE + 3 {
            diagnostic(&source, &format!("note-{index}"));
        }
        source
            .put_document(
                "doc/backup-example",
                b"# Example\n\nA durable document.\n",
                &None,
                "backup-document",
            )
            .unwrap();
        let backup_path = root.path().join("graph.jsonl");
        let header = archive(&source, &backup_path);
        let path = root.path().join("restored.db");
        let restored = backup::restore(&backup_path, &path).unwrap();
        assert!(restored.projections_match);
        assert_eq!(restored.graph_digest, header.graph_digest);
        assert!(Store::open(&path, "alder").is_err());
        let target = Store::open(&path, &restored.writer).unwrap();
        assert_eq!(
            source.replication_inventory().unwrap().digest,
            target.replication_inventory().unwrap().digest
        );
        let left = source
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        let right = target
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        assert_eq!(
            serde_json::to_value(left.envelopes).unwrap(),
            serde_json::to_value(right.envelopes).unwrap()
        );
        diagnostic(&target, "fresh writer");
        assert_eq!(target.writer_head(&restored.writer).unwrap().unwrap().0, 1);
        drop(target);
        assert!(
            backup::restore(&backup_path, &path)
                .unwrap_err()
                .to_string()
                .contains("already holds claims")
        );
    }

    #[test]
    fn an_upgrade_may_project_more_claims_but_must_preserve_the_exact_log() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "newly readable after upgrade");
        let path = root.path().join("older-registry.jsonl");
        let header = archive(&source, &path);
        // Model an old reader's header: it held this envelope but did not yet know its kind.
        let old_sources = Store::open_memory("empty")
            .unwrap()
            .backup_header()
            .unwrap()
            .tables["claim_sources"]
            .clone();
        rewrite(&path, |records| {
            if let Record::Header(header) = &mut records[0] {
                header.registry_digest = "older-registry".into();
                header.tables.insert("claim_sources".into(), old_sources);
                header.graph_digest = "older-projections".into();
            }
        });
        let report = backup::restore(&path, &root.path().join("upgraded.db")).unwrap();
        assert_eq!(report.log_digest, header.log_digest);
        assert!(!report.projections_match);
        assert_eq!(report.tables["claim_sources"].count, 1);
        rewrite(&path, |records| {
            records.retain(|record| !matches!(record, Record::Envelope(_)));
        });
        assert!(
            backup::restore(&path, &root.path().join("missing.db"))
                .unwrap_err()
                .to_string()
                .contains("log digest")
        );
    }

    #[test]
    fn restore_replaces_an_offline_empty_database_and_can_backup_it_again() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "note");
        let path = root.path().join("source.jsonl");
        let header = archive(&source, &path);
        let target = root.path().join("empty.db");
        drop(Store::open(&target, "unused").unwrap());
        let report = backup::restore(&path, &target).unwrap();
        assert_eq!(report.graph_digest, header.graph_digest);
        let second =
            backup::create_from_database(&target, &root.path().join("again.jsonl")).unwrap();
        assert_eq!(second.log_digest, header.log_digest);
    }

    #[test]
    fn restore_refuses_an_empty_database_still_open_by_a_node() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "note");
        let path = root.path().join("source.jsonl");
        archive(&source, &path);
        let destination = root.path().join("live.db");
        let live = Store::open(&destination, "birch").unwrap();
        assert!(backup::restore(&path, &destination).is_err());
        assert!(live.claims_for("daemon/alder", None).unwrap().is_empty());
        diagnostic(&live, "still serving");
        assert_eq!(live.claims_for("daemon/birch", None).unwrap().len(), 1);
    }

    #[test]
    fn a_backup_from_an_older_registry_preserves_and_later_projects_unknown_claims() {
        let root = tempfile::tempdir().unwrap();
        let current = Store::open_memory("alder").unwrap();
        diagnostic(&current, "a newer kind");
        let older = Store::open_memory("birch").unwrap();
        let mut registry = st3_schema::registry().clone();
        registry.claims.remove("daemon.diagnostic").unwrap();
        older.set_claim_registry(registry);
        let exchange = current
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        older
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        assert_eq!(older.validate_replication_backlog().unwrap().unknown, 1);
        older.project_replication_backlog().unwrap();
        let archive_path = root.path().join("older.jsonl");
        let header = archive(&older, &archive_path);
        assert_eq!(header.tables["claim_sources"].count, 0);
        let report = backup::restore(&archive_path, &root.path().join("upgraded.db")).unwrap();
        assert_eq!(report.log_digest, header.log_digest);
        assert_eq!(report.tables["claim_sources"].count, 1);
        assert!(!report.projections_match);
    }

    #[test]
    fn an_unprojected_snapshot_is_refused_before_any_archive_bytes_are_written() {
        let root = tempfile::tempdir().unwrap();
        let writer = Store::open_memory("alder").unwrap();
        diagnostic(&writer, "not projected yet");
        let database = root.path().join("source.db");
        let source = Store::open(&database, "birch").unwrap();
        let worker = Store::open(&database, "birch").unwrap();
        let exchange = writer
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        worker
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        worker.validate_replication_backlog().unwrap();
        let mut output = Vec::new();
        let error = source.write_backup(&mut output).unwrap_err();
        assert!(error.to_string().contains("fully projected"), "{error:#}");
        assert!(output.is_empty());
        diagnostic(
            &source,
            "a local write must not hide the pending projection",
        );
        assert!(source.write_backup(&mut output).is_err());
        assert!(output.is_empty());
        worker.project_replication_backlog().unwrap();
        let path = root.path().join("projected.jsonl");
        let header = archive(&source, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(header.graph_digest, report.graph_digest);
    }

    #[test]
    fn offline_export_finishes_upgrade_recovery_before_recording_digests() {
        let root = tempfile::tempdir().unwrap();
        let current = Store::open_memory("alder").unwrap();
        diagnostic(&current, "a newer kind");
        let path = root.path().join("older.db");
        let older = Store::open(&path, "birch").unwrap();
        let mut registry = st3_schema::registry().clone();
        registry.claims.remove("daemon.diagnostic").unwrap();
        older.set_claim_registry(registry);
        let exchange = current
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        older
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        assert_eq!(older.validate_replication_backlog().unwrap().unknown, 1);
        older.project_replication_backlog().unwrap();
        drop(older);
        let archive_path = root.path().join("upgraded.jsonl");
        let header = backup::create_from_database(&path, &archive_path).unwrap();
        assert_eq!(header.tables["claim_sources"].count, 1);
        let report = backup::restore(&archive_path, &root.path().join("restored.db")).unwrap();
        assert!(report.projections_match);
    }

    #[test]
    fn restore_keeps_history_admitted_before_a_later_keyed_membership() {
        let root = tempfile::tempdir().unwrap();
        let legacy = Store::open_memory("alder").unwrap();
        diagnostic(&legacy, "history already admitted");
        let source = Store::open_memory("birch").unwrap();
        let key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        source.bind_fleet("test-fleet").unwrap();
        source.pin_fleet_anchor(key.public()).unwrap();
        source.set_member_key(Some(key.clone())).unwrap();
        source
            .admit_fleet_anchor("test-fleet", key.public(), "listening")
            .unwrap();
        let exchange = legacy
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        source
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        source.validate_replication_backlog().unwrap();
        source.project_replication_backlog().unwrap();
        let later_key = crate::fleet::MemberKey::generate().unwrap().0;
        source
            .append_claim(&ClaimInput {
                subject: "host/alder".into(),
                kind: "fleet.member-admitted".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("fleet_id".into(), json!("test-fleet")),
                    ("member_key".into(), json!(later_key.public())),
                    ("via".into(), json!("invite")),
                    ("sponsor".into(), json!("host/birch")),
                    ("mode".into(), json!("listening")),
                    ("writer_floor".into(), json!(0)),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        source.replication_snapshot().unwrap();
        assert!(!source.fleet_admission_residue().unwrap().is_empty());
        let archive_path = root.path().join("history.jsonl");
        let header = archive(&source, &archive_path);
        let report = backup::restore(&archive_path, &root.path().join("restored.db")).unwrap();
        assert!(report.projections_match);
        assert_eq!(report.log_digest, header.log_digest);
    }

    #[test]
    fn a_backup_reapplies_replicated_repairs_before_comparing_digests() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "original");
        diagnostic(&source, "replacement");
        let receiver = Store::open_memory("birch").unwrap();
        let exchange = source
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        receiver
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        receiver.validate_replication_backlog().unwrap();
        receiver.project_replication_backlog().unwrap();
        let claims = receiver.claims_for("daemon/alder", None).unwrap();
        let repaired = &claims[0].id;
        let replacement = &claims[1].id;
        let record = receiver
            .replica_records(false)
            .unwrap()
            .into_iter()
            .find(|record| record.claim_id.as_deref() == Some(repaired.as_str()))
            .unwrap();
        receiver
            .connection
            .write()
            .execute(
                "UPDATE replica_records SET state='invalid' WHERE record_ref=?1",
                [&record.record_ref],
            )
            .unwrap();
        receiver
            .repair_replica_record(
                &record.record_ref,
                replacement,
                "fixture repair",
                "person/operator",
                "backup-repair",
            )
            .unwrap();
        receiver.replay_replication_graph().unwrap();
        let path = root.path().join("repaired.jsonl");
        let header = archive(&receiver, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.tables, header.tables);
        assert!(report.projections_match);
    }

    #[test]
    fn a_backup_preserves_multiple_wire_envelopes_for_the_same_batch() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "note");
        let mut exchange = source
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        let mut variant = exchange.envelopes[0].clone();
        let mut payload: smallclaims::claim::ReplicaEnvelopePayload =
            ciborium::from_reader(variant.payload.bytes().unwrap()).unwrap();
        let blob = b"another wire representation".to_vec();
        payload
            .blobs
            .insert(hex::encode(Sha256::digest(&blob)), blob);
        let mut bytes = Vec::new();
        ciborium::into_writer(&payload, &mut bytes).unwrap();
        variant.hash = smallclaims::hash::replica_envelope_hash(
            &variant.writer,
            variant.sequence,
            variant.previous_hash.as_deref(),
            variant.accepted_at_unix_ms,
            &bytes,
        );
        variant.payload = bytes.into();
        exchange.envelopes.push(variant);
        let receiver = Store::open_memory("birch").unwrap();
        receiver
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        receiver.validate_replication_backlog().unwrap();
        receiver.project_replication_backlog().unwrap();
        let path = root.path().join("variants.jsonl");
        let header = archive(&receiver, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.envelopes, 2);
        assert_eq!(report.log_digest, header.log_digest);
        assert!(report.projections_match);
    }

    #[test]
    fn create_refuses_a_partial_writer_chain_until_the_missing_envelope_arrives() {
        let root = tempfile::tempdir().unwrap();
        let writer = Store::open_memory("alder").unwrap();
        diagnostic(&writer, "first");
        diagnostic(&writer, "second");
        let source = Store::open_memory("birch").unwrap();
        let mut exchange = writer
            .export_replication_exchange("test-fleet", &ReplicationInventory::default())
            .unwrap();
        let first = exchange.envelopes.remove(0);
        source
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        source.validate_replication_backlog().unwrap();
        source.project_replication_backlog().unwrap();
        let error = source.write_backup(&mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("predecessor"), "{error:#}");
        exchange.envelopes = vec![first];
        source
            .receive_replication_exchange("alder", "test-fleet", &exchange)
            .unwrap();
        source.validate_replication_backlog().unwrap();
        source.project_replication_backlog().unwrap();
        let path = root.path().join("complete.jsonl");
        let header = archive(&source, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.graph_digest, header.graph_digest);
    }

    #[test]
    fn restore_rejects_a_missing_writer_chain_predecessor() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "before");
        diagnostic(&source, "after");
        let path = root.path().join("chain.jsonl");
        archive(&source, &path);
        rewrite(&path, |records| {
            records.retain(
                |record| !matches!(record, Record::Envelope(envelope) if envelope.sequence == 1),
            )
        });
        let destination = root.path().join("restored.db");
        let error = backup::restore(&path, &destination).unwrap_err();
        assert!(error.to_string().contains("predecessor"), "{error:#}");
        assert!(!destination.exists());
    }

    #[test]
    fn a_trimmed_backup_restores_verified_tombstones_and_the_same_inventory() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open(&root.path().join("source.db"), "alder").unwrap();
        source.bind_fleet("test-fleet").unwrap();
        for index in 0..12 {
            diagnostic(&source, &format!("note-{index}"));
        }
        let context = CheckpointContext {
            now_unix_ms: now_ms() + 3 * 86_400_000,
            configured_peers: vec![],
            scratch: root.path().join("scratch"),
            reviewer: "person/operator".into(),
        };
        for _ in 0..3 {
            source.checkpoint_step(&context).unwrap();
        }
        assert!(source.trimmed_checkpoint().unwrap().is_some());
        assert!(source.checkpointed_envelopes().unwrap() > 0);
        assert_eq!(
            source
                .restore_checkpoint_history(&CheckpointManifest::default())
                .unwrap_err()
                .code,
            "checkpoint-history-exists"
        );
        let path = root.path().join("trimmed.jsonl");
        let header = archive(&source, &path);
        let target = root.path().join("restored.db");
        let report = backup::restore(&path, &target).unwrap();
        assert_eq!(report.graph_digest, header.graph_digest);
        let restored = Store::open(&target, &report.writer).unwrap();
        assert_eq!(
            source.replication_inventory().unwrap().digest,
            restored.replication_inventory().unwrap().digest
        );
        assert_eq!(
            source.checkpointed_envelopes().unwrap(),
            restored.checkpointed_envelopes().unwrap()
        );
    }

    #[test]
    fn a_backup_during_an_interrupted_trim_preserves_the_complete_log() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        source.bind_fleet("test-fleet").unwrap();
        for index in 0..12 {
            diagnostic(&source, &format!("note-{index}"));
        }
        let context = CheckpointContext {
            now_unix_ms: now_ms() + 3 * 86_400_000,
            configured_peers: vec![],
            scratch: root.path().join("scratch"),
            reviewer: "person/operator".into(),
        };
        for _ in 0..2 {
            source.checkpoint_step(&context).unwrap();
        }
        source.set_trim_fault(Some(TrimFault::AfterChunk(1)));
        assert!(source.checkpoint_step(&context).is_err());
        assert!(source.trimmed_checkpoint().unwrap().is_none());
        assert!(source.checkpointed_envelopes().unwrap() > 0);
        let path = root.path().join("trimming.jsonl");
        let header = archive(&source, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.log_digest, header.log_digest);
        assert!(report.projections_match);
    }

    #[test]
    fn a_backup_keeps_trimmed_history_when_a_newer_certificate_is_not_applied_yet() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        source.bind_fleet("test-fleet").unwrap();
        for index in 0..12 {
            diagnostic(&source, &format!("note-{index}"));
        }
        let mut context = CheckpointContext {
            now_unix_ms: now_ms() + 3 * 86_400_000,
            configured_peers: vec![],
            scratch: root.path().join("scratch"),
            reviewer: "person/operator".into(),
        };
        for _ in 0..3 {
            source.checkpoint_step(&context).unwrap();
        }
        let applied = source.trimmed_checkpoint().unwrap().unwrap();
        diagnostic(&source, "after the first trim");
        context.now_unix_ms += 86_400_000;
        for _ in 0..2 {
            source.checkpoint_step(&context).unwrap();
        }
        assert_eq!(source.trimmed_checkpoint().unwrap().unwrap(), applied);
        assert!(
            source
                .checkpoint_status(context.now_unix_ms, &context.configured_peers)
                .unwrap()
                .newest_stable
                .unwrap()
                .terms
                .cut_unix_ms
                > applied.cut_unix_ms
        );
        let path = root.path().join("certified.jsonl");
        let header = archive(&source, &path);
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.log_digest, header.log_digest);
        assert!(report.projections_match);
    }

    #[test]
    fn a_corrupt_or_truncated_backup_never_publishes_a_database() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        diagnostic(&source, "note");
        let path = root.path().join("graph.jsonl");
        archive(&source, &path);
        let bytes = fs::read(&path).unwrap();
        for corrupted in [bytes[..bytes.len() - 3].to_vec(), {
            let mut bytes = bytes.clone();
            let position = bytes.iter().position(|byte| *byte == b'0').unwrap();
            bytes[position] = b'1';
            bytes
        }] {
            fs::write(&path, corrupted).unwrap();
            let target = root.path().join("target.db");
            assert!(backup::restore(&path, &target).is_err());
            assert!(!target.exists());
        }
    }

    fn rewrite(path: &Path, mutate: impl FnOnce(&mut Vec<Record>)) {
        let mut input = std::io::BufReader::new(fs::File::open(path).unwrap());
        let mut records = Vec::new();
        loop {
            let mut line = String::new();
            use std::io::BufRead;
            if input.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let record: Record = serde_json::from_str(&line).unwrap();
            if !matches!(record, Record::End { .. }) {
                records.push(record);
            }
        }
        mutate(&mut records);
        let count = records
            .iter()
            .filter(|r| matches!(r, Record::Envelope(_)))
            .count() as u64;
        let mut output = fs::File::create(path).unwrap();
        let mut hash = Sha256::new();
        for record in records {
            write_record(&mut output, &mut hash, &record).unwrap();
        }
        serde_json::to_writer(
            &mut output,
            &Record::End {
                envelopes: count,
                sha256: hex::encode(hash.finalize()),
            },
        )
        .unwrap();
        output.write_all(b"\n").unwrap();
    }

    #[test]
    fn checksummed_tampering_still_fails_signature_and_graph_verification() {
        let root = tempfile::tempdir().unwrap();
        let source = Store::open_memory("alder").unwrap();
        let key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
        source.bind_fleet("test-fleet").unwrap();
        source.set_member_key(Some(key.clone())).unwrap();
        diagnostic(&source, "note");
        let path = root.path().join("graph.jsonl");
        archive(&source, &path);
        rewrite(&path, |records| {
            for record in records {
                if let Record::Envelope(envelope) = record {
                    envelope.signature = Some("invalid".into());
                }
            }
        });
        assert!(
            backup::restore(&path, &root.path().join("bad-signature.db"))
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
        archive(&source, &path);
        rewrite(&path, |records| {
            if let Record::Header(header) = &mut records[0] {
                header.graph_digest = "changed".into();
            }
        });
        assert!(
            backup::restore(&path, &root.path().join("bad-digest.db"))
                .unwrap_err()
                .to_string()
                .contains("graph digest")
        );
    }

    #[test]
    fn backup_snapshot_does_not_include_a_concurrent_write() {
        let root = tempfile::tempdir().unwrap();
        let source = Arc::new(Store::open(&root.path().join("source.db"), "alder").unwrap());
        diagnostic(&source, "before");
        struct WriteDuringBackup {
            bytes: Vec<u8>,
            store: Arc<Store>,
            wrote: bool,
        }
        impl Write for WriteDuringBackup {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.wrote {
                    self.wrote = true;
                    let store = self.store.clone();
                    std::thread::spawn(move || diagnostic(&store, "after"))
                        .join()
                        .unwrap();
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = WriteDuringBackup {
            bytes: vec![],
            store: source.clone(),
            wrote: false,
        };
        let header = source.write_backup(&mut output).unwrap();
        let path = root.path().join("snapshot.jsonl");
        fs::write(&path, output.bytes).unwrap();
        let report = backup::restore(&path, &root.path().join("restored.db")).unwrap();
        assert_eq!(report.graph_digest, header.graph_digest);
        assert_eq!(report.tables["claim_sources"].count, 1);
        assert_eq!(
            source.backup_header().unwrap().tables["claim_sources"].count,
            2
        );
    }
}

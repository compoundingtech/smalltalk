//! Tombstones: what a node keeps of the envelopes and claims a checkpoint dropped, and the
//! manifest a node that did not take part adopts them from. Inventories list a dropped
//! envelope's identity, so the authority digest does not change and no peer sends it back.
//! Evidence checks, ancestry walks and idempotent retries read a dropped claim's tombstone.

use super::*;

/// The most tombstones one page of a checkpoint manifest carries.
pub const CHECKPOINT_MANIFEST_PAGE_LIMIT: usize = 10_000;

/// Every tombstone a node holds for one checkpoint and the checkpoints before it: what a node
/// that did not take part needs to adopt it. Its `drop_digest` must equal the one every
/// participant verified.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
}

/// Where a manifest page starts: after this envelope identity, or, once every envelope has been
/// listed, after this claim ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "part", content = "after")]
pub enum CheckpointManifestCursor {
    Envelope(EnvelopeKey),
    Claim(String),
}

/// One page of a checkpoint's manifest, as `/v1/peer/checkpoint` asks for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestRequest {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    #[serde(default)]
    pub after: Option<CheckpointManifestCursor>,
}

/// One page of a manifest. `next` is set while more tombstones follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifestPage {
    pub checkpoint: String,
    pub cut_unix_ms: u128,
    pub envelopes: Vec<EnvelopeTombstone>,
    pub claims: Vec<ClaimTombstone>,
    #[serde(default)]
    pub next: Option<CheckpointManifestCursor>,
}

impl CheckpointManifest {
    /// Add the next page of this manifest, and return where the following page starts.
    pub fn append(
        &mut self,
        page: CheckpointManifestPage,
    ) -> Result<Option<CheckpointManifestCursor>, St3Error> {
        if page.checkpoint != self.checkpoint || page.cut_unix_ms != self.cut_unix_ms {
            return Err(St3Error::new(
                "checkpoint-manifest-invalid",
                format!(
                    "a manifest page for `{}` does not continue `{}`",
                    page.checkpoint, self.checkpoint
                ),
            ));
        }
        self.envelopes.extend(page.envelopes);
        self.claims.extend(page.claims);
        Ok(page.next)
    }
}

fn invalid_manifest(message: impl Into<String>) -> St3Error {
    St3Error::new("checkpoint-manifest-invalid", message)
}

/// Check a manifest against the `drop_digest` that a stable checkpoint's participants verified,
/// before anything from it is stored. The digest covers every field of every tombstone, so a
/// changed field, or a tombstone added or left out, is rejected here.
pub fn verify_checkpoint_manifest(
    manifest: &CheckpointManifest,
    expected_drop_digest: &str,
) -> Result<(), St3Error> {
    let mut envelopes = BTreeSet::new();
    for envelope in &manifest.envelopes {
        if envelope.accepted_at_unix_ms >= manifest.cut_unix_ms {
            return Err(invalid_manifest(format!(
                "envelope {}/{} is not before the cut",
                envelope.writer, envelope.sequence
            )));
        }
        if !envelopes.insert((
            envelope.writer.as_str(),
            envelope.sequence,
            envelope.envelope_hash.as_str(),
        )) {
            return Err(invalid_manifest(format!(
                "envelope {}/{} is listed twice",
                envelope.writer, envelope.sequence
            )));
        }
    }
    let mut claims = BTreeSet::new();
    for claim in &manifest.claims {
        // A checkpoint drops a claim only with its whole envelope.
        if !envelopes.contains(&(
            claim.writer.as_str(),
            claim.sequence,
            claim.envelope_hash.as_str(),
        )) {
            return Err(invalid_manifest(format!(
                "claim `{}` names an envelope the manifest keeps",
                claim.id
            )));
        }
        if claim.accepted_at_unix_ms >= manifest.cut_unix_ms {
            return Err(invalid_manifest(format!(
                "claim `{}` is not before the cut",
                claim.id
            )));
        }
        if !claims.insert(claim.id.as_str()) {
            return Err(invalid_manifest(format!(
                "claim `{}` is listed twice",
                claim.id
            )));
        }
    }
    let digest = drop_digest(&manifest.envelopes, &manifest.claims);
    if digest != expected_drop_digest {
        return Err(St3Error::new(
            "checkpoint-manifest-mismatch",
            format!(
                "the manifest of `{}` does not match the drop its participants verified",
                manifest.checkpoint
            ),
        )
        .with_detail("drop_digest", digest)
        .with_detail("expected_drop_digest", expected_drop_digest.to_owned()));
    }
    Ok(())
}

fn row_millis(value: i64) -> u128 {
    u128::try_from(value).unwrap_or_default()
}

/// Whether a claim is stored here, or was and a checkpoint dropped it. Evidence may cite either.
pub(crate) fn claim_or_tombstone_exists(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1)
                 OR EXISTS(SELECT 1 FROM checkpoint_claims WHERE id=?1)",
        )?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a checkpoint dropped this claim here.
pub(crate) fn claim_tombstoned(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM checkpoint_claims WHERE id=?1)")?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a checkpoint dropped this envelope here.
pub(crate) fn envelope_tombstoned(
    connection: &Connection,
    writer: &str,
    sequence: u64,
    envelope_hash: &str,
) -> Result<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM checkpoint_envelopes
                           WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3)",
        )?
        .query_row(params![writer, sequence, envelope_hash], |row| row.get(0))?)
}

/// The request digest and claim ID of each dropped claim of an operation.
pub(crate) fn checkpointed_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Vec<(String, String)>> {
    connection
        .prepare_cached(
            "SELECT request_digest, id FROM checkpoint_claims
             WHERE operation_id=?1 AND request_digest IS NOT NULL
             ORDER BY request_digest, id",
        )?
        .query_map([operation_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Every operation with a dropped claim, as `(operation, request digest, claim)`.
pub(crate) fn checkpointed_operations(
    connection: &Connection,
) -> Result<Vec<(String, String, String)>> {
    connection
        .prepare_cached(
            "SELECT operation_id, request_digest, id FROM checkpoint_claims
             WHERE operation_id IS NOT NULL AND request_digest IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

impl Store {
    /// One page of the manifest of a checkpoint: the tombstones this node holds for it and for
    /// every earlier checkpoint, envelopes first, each in sorted order. Checkpoints are named
    /// for UTC days, so their names sort in the order of their cuts.
    pub fn checkpoint_manifest_page(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.manifest_page(request, CHECKPOINT_MANIFEST_PAGE_LIMIT)
    }

    fn manifest_page(
        &self,
        request: &CheckpointManifestRequest,
        limit: usize,
    ) -> Result<CheckpointManifestPage> {
        let connection = self.readers.get();
        // One snapshot for the page, so a trim that commits meanwhile cannot tear it.
        let transaction = connection.unchecked_transaction()?;
        let mut page = CheckpointManifestPage {
            checkpoint: request.checkpoint.clone(),
            cut_unix_ms: request.cut_unix_ms,
            envelopes: Vec::new(),
            claims: Vec::new(),
            next: None,
        };
        let after_claim = match &request.after {
            None | Some(CheckpointManifestCursor::Envelope(_)) => {
                let (writer, sequence, hash) = match &request.after {
                    Some(CheckpointManifestCursor::Envelope(key)) => (
                        key.writer.as_str(),
                        i64::try_from(key.sequence).unwrap_or(i64::MAX),
                        key.envelope_hash.as_str(),
                    ),
                    _ => ("", i64::MIN, ""),
                };
                page.envelopes = transaction
                    .prepare_cached(
                        "SELECT writer, sequence, envelope_hash, accepted_at_unix_ms
                         FROM checkpoint_envelopes
                         WHERE checkpoint<=?1 AND (writer, sequence, envelope_hash) > (?2, ?3, ?4)
                         ORDER BY writer, sequence, envelope_hash LIMIT ?5",
                    )?
                    .query_map(
                        params![request.checkpoint, writer, sequence, hash, limit + 1],
                        |row| {
                            Ok(EnvelopeTombstone {
                                writer: row.get(0)?,
                                sequence: row.get(1)?,
                                envelope_hash: row.get(2)?,
                                accepted_at_unix_ms: row_millis(row.get(3)?),
                            })
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if page.envelopes.len() > limit {
                    page.envelopes.truncate(limit);
                    let last = page.envelopes.last().expect("the page is not empty");
                    page.next = Some(CheckpointManifestCursor::Envelope(EnvelopeKey {
                        writer: last.writer.clone(),
                        sequence: last.sequence,
                        envelope_hash: last.envelope_hash.clone(),
                    }));
                    return Ok(page);
                }
                String::new()
            }
            Some(CheckpointManifestCursor::Claim(after)) => after.clone(),
        };
        let room = limit - page.envelopes.len();
        page.claims = transaction
            .prepare_cached(
                "SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at_unix_ms
                 FROM checkpoint_claims WHERE checkpoint<=?1 AND id>?2
                 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![request.checkpoint, after_claim, room + 1], |row| {
                Ok((
                    ClaimTombstone {
                        id: row.get(0)?,
                        writer: row.get(1)?,
                        sequence: row.get(2)?,
                        envelope_hash: row.get(3)?,
                        subject: row.get(4)?,
                        kind: row.get(5)?,
                        actor: row.get(6)?,
                        predecessors: Vec::new(),
                        operation_id: row.get(8)?,
                        request_digest: row.get(9)?,
                        accepted_at_unix_ms: row_millis(row.get(10)?),
                    },
                    row.get::<_, String>(7)?,
                ))
            })?
            .map(|row| {
                let (mut claim, predecessors) = row?;
                claim.predecessors = serde_json::from_str(&predecessors)?;
                Ok(claim)
            })
            .collect::<Result<Vec<_>>>()?;
        if page.claims.len() > room {
            page.claims.truncate(room);
            if let Some(last) = page.claims.last() {
                page.next = Some(CheckpointManifestCursor::Claim(last.id.clone()));
            } else {
                page.next = Some(CheckpointManifestCursor::Claim(after_claim));
            }
        }
        Ok(page)
    }

    /// The whole manifest of a checkpoint, read page by page.
    pub fn checkpoint_manifest(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
    ) -> Result<CheckpointManifest> {
        self.read_manifest(checkpoint, cut_unix_ms, CHECKPOINT_MANIFEST_PAGE_LIMIT)
    }

    fn read_manifest(
        &self,
        checkpoint: &str,
        cut_unix_ms: u128,
        limit: usize,
    ) -> Result<CheckpointManifest> {
        let mut manifest = CheckpointManifest {
            checkpoint: checkpoint.to_owned(),
            cut_unix_ms,
            ..CheckpointManifest::default()
        };
        let mut after = None;
        loop {
            let request = CheckpointManifestRequest {
                checkpoint: checkpoint.to_owned(),
                cut_unix_ms,
                after,
            };
            let page = self.manifest_page(&request, limit)?;
            after = manifest.append(page)?;
            if after.is_none() {
                return Ok(manifest);
            }
        }
    }

    /// Record a checkpoint's tombstones and delete the rows they stand for, in one transaction.
    /// A trim deletes in chunks with `record_checkpoint_tombstones_tx` and
    /// `delete_dropped_rows_tx` instead, and calls `replica_rows_changed` after each.
    #[cfg_attr(not(test), allow(dead_code))] // Adoption calls it (P5).
    pub(crate) fn apply_checkpoint_drop(
        &self,
        checkpoint: &str,
        envelopes: &[EnvelopeTombstone],
        claims: &[ClaimTombstone],
    ) -> Result<()> {
        let mut connection = self.connection.write();
        let transaction = connection.transaction()?;
        record_checkpoint_tombstones_tx(&transaction, checkpoint, envelopes, claims)?;
        delete_dropped_rows_tx(&transaction, envelopes, claims)?;
        transaction.commit()?;
        drop(connection);
        self.replica_rows_changed();
        Ok(())
    }

    /// Envelope rows changed without a new store index, as when a checkpoint records tombstones
    /// or deletes dropped rows. The next replication snapshot reads the inventory again.
    #[cfg_attr(not(test), allow(dead_code))] // The trim calls it (P5).
    pub(crate) fn replica_rows_changed(&self) {
        self.replica_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether evidence may cite this claim: it is stored, or a checkpoint dropped it.
    pub fn evidence_exists(&self, id: &str) -> Result<bool> {
        claim_or_tombstone_exists(&self.readers.get(), id)
    }

    /// How many envelopes checkpoints have dropped here.
    pub fn checkpointed_envelopes(&self) -> Result<u64> {
        let connection = self.readers.get();
        Ok(
            connection.query_row("SELECT COUNT(*) FROM checkpoint_envelopes", [], |row| {
                row.get(0)
            })?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLEET: &str = "fleet/test";
    const CHECKPOINT: &str = "checkpoint/2026-09-27";

    fn diagnostic(subject: &str, n: usize, reason: &str) -> ClaimInput {
        ClaimInput {
            subject: subject.into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), json!("warning")),
                ("code".into(), json!("slow-request")),
                ("reason".into(), json!(reason)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("{subject}-diagnostic-{n}")),
        }
    }

    fn write_diagnostics(store: &Store, subject: &str, count: usize) {
        for n in 0..count {
            store
                .append_claim(&diagnostic(subject, n, &format!("slow {n}")))
                .unwrap();
        }
    }

    /// A store in a fleet whose checkpoint drops all but the newest of its diagnostics.
    fn store_with_drops(origin: &str) -> (Store, DropPlan) {
        let store = Store::open_memory(origin).unwrap();
        store.bind_fleet(FLEET).unwrap();
        write_diagnostics(&store, "daemon/alder", 5);
        let plan = plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
        assert!(plan.envelopes.len() >= 3, "{plan:?}");
        (store, plan)
    }

    fn trim(store: &Store, plan: &DropPlan) {
        store
            .apply_checkpoint_drop(CHECKPOINT, &plan.envelopes, &plan.claims)
            .unwrap();
    }

    /// Send `to` what `from` holds and `to` lacks, as one exchange does. With `buckets`, `to`
    /// describes itself with range digests; without, with its complete inventory, as an older
    /// build does.
    fn sync(from: &Store, from_name: &str, to: &Store, buckets: bool) -> ReplicationReceipt {
        let remote = if buckets {
            to.export_replication_summary(FLEET).unwrap().inventory
        } else {
            to.replication_inventory().unwrap()
        };
        let exchange = from.export_replication_exchange(FLEET, &remote).unwrap();
        let receipt = to
            .receive_replication_exchange(from_name, FLEET, &exchange)
            .unwrap();
        to.validate_replication_backlog().unwrap();
        to.project_replication_backlog().unwrap();
        receipt
    }

    fn authority(store: &Store) -> String {
        store.replication_inventory().unwrap().digest
    }

    fn key(writer: &str, sequence: u64, hash: &str) -> EnvelopeKey {
        EnvelopeKey {
            writer: writer.into(),
            sequence,
            envelope_hash: hash.into(),
        }
    }

    fn held(store: &Store) -> BTreeSet<EnvelopeKey> {
        store
            .readers
            .get()
            .prepare("SELECT writer, sequence, envelope_hash FROM replica_envelopes")
            .unwrap()
            .query_map([], |row| {
                Ok(EnvelopeKey {
                    writer: row.get(0)?,
                    sequence: row.get(1)?,
                    envelope_hash: row.get(2)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn dropped_keys(plan: &DropPlan) -> BTreeSet<EnvelopeKey> {
        plan.envelopes
            .iter()
            .map(|envelope| key(&envelope.writer, envelope.sequence, &envelope.envelope_hash))
            .collect()
    }

    fn sent_keys(exchange: &ReplicationExchange) -> BTreeSet<EnvelopeKey> {
        exchange
            .envelopes
            .iter()
            .map(|envelope| key(&envelope.writer, envelope.sequence, &envelope.hash))
            .collect()
    }

    fn identity_keys(identities: &[ReplicaEnvelopeId]) -> BTreeSet<EnvelopeKey> {
        identities
            .iter()
            .map(|identity| key(&identity.writer, identity.sequence, &identity.hash))
            .collect()
    }

    #[test]
    fn a_trim_keeps_every_identity_and_the_authority_digest() {
        let (store, plan) = store_with_drops("alder");
        let before = authority(&store);
        let held_before = held(&store);
        let dropped = dropped_keys(&plan);

        // Tombstones first, as a trim writes them: nothing is deleted yet.
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            record_checkpoint_tombstones_tx(
                &transaction,
                CHECKPOINT,
                &plan.envelopes,
                &plan.claims,
            )
            .unwrap();
            transaction.commit().unwrap();
        }
        store.replica_rows_changed();
        assert_eq!(authority(&store), before);
        assert_eq!(held(&store), held_before);

        // Then the rows go. Recording the tombstones again changes nothing.
        trim(&store, &plan);
        assert_eq!(authority(&store), before);
        assert_eq!(
            held(&store),
            held_before.difference(&dropped).cloned().collect()
        );
        for claim in &plan.claims {
            assert!(store.claim_by_id(&claim.id).unwrap().is_none());
        }
        assert_eq!(
            store.checkpointed_envelopes().unwrap(),
            plan.envelopes.len() as u64
        );
        assert_eq!(
            store
                .replication_status(true, Some(FLEET), &[])
                .unwrap()
                .checkpointed_envelopes,
            plan.envelopes.len() as u64
        );
        // A full read of the inventory agrees with the snapshot the store kept.
        let (full, _) = full_compact_replication_inventory(&store.readers.get()).unwrap();
        assert_eq!(full.digest, before);
    }

    #[test]
    fn export_lists_tombstoned_identities_but_never_sends_them() {
        let (store, plan) = store_with_drops("alder");
        trim(&store, &plan);
        let dropped = dropped_keys(&plan);
        let empty = Store::open_memory("birch").unwrap();
        empty.bind_fleet(FLEET).unwrap();

        // A peer without range digests gets the complete inventory, tombstones included.
        let full = store
            .export_replication_exchange(FLEET, &empty.replication_inventory().unwrap())
            .unwrap();
        assert!(dropped.is_subset(&identity_keys(&full.inventory.envelopes)));
        assert!(sent_keys(&full).is_disjoint(&dropped));
        assert_eq!(sent_keys(&full), held(&store));
        // The same for an empty request.
        let first = store
            .export_replication_exchange(FLEET, &ReplicationInventory::default())
            .unwrap();
        assert!(sent_keys(&first).is_disjoint(&dropped));

        // A peer with range digests gets the differing ranges listed, tombstones included.
        let ranged = store
            .export_replication_exchange(
                FLEET,
                &empty.export_replication_summary(FLEET).unwrap().inventory,
            )
            .unwrap();
        assert!(dropped.is_subset(&identity_keys(&ranged.inventory.envelopes)));
        assert!(sent_keys(&ranged).is_disjoint(&dropped));
        assert_eq!(sent_keys(&ranged), held(&store));

        // The new node ends with every kept envelope and none of the dropped ones. It lacks the
        // tombstones, so its inventory differs until it adopts the checkpoint.
        while sync(&store, "alder", &empty, true).received != 0 {}
        assert_eq!(held(&empty), held(&store));
        assert_ne!(authority(&empty), authority(&store));
        for claim in &plan.claims {
            assert!(empty.claim_by_id(&claim.id).unwrap().is_none());
        }
    }

    #[test]
    fn a_differing_range_lists_its_tombstones_and_sends_only_payloads() {
        let (store, plan) = store_with_drops("alder");
        let peer = Store::open_memory("birch").unwrap();
        peer.bind_fleet(FLEET).unwrap();
        // The peer holds the oldest envelope only, so the writer's range differs.
        let exchange = store
            .export_replication_exchange(FLEET, &ReplicationInventory::default())
            .unwrap();
        let oldest = exchange
            .envelopes
            .iter()
            .min_by_key(|envelope| envelope.sequence)
            .unwrap();
        let only_oldest = ReplicationExchange {
            envelopes: vec![oldest.clone()],
            ..exchange.clone()
        };
        peer.receive_replication_exchange("alder", FLEET, &only_oldest)
            .unwrap();
        trim(&store, &plan);
        let snapshot = store.replication_snapshot().unwrap();
        // The peer's range digests, and its listing of the range, as its answer carries it.
        let mut remote = peer.export_replication_summary(FLEET).unwrap().inventory;
        assert!(!remote.buckets.is_empty());
        remote.envelopes = peer.replication_inventory().unwrap().envelopes;
        let (missing, listed) = compact_replication_difference(
            &snapshot.inventory,
            &snapshot.buckets,
            &remote,
            REPLICATION_EXCHANGE_ENVELOPE_LIMIT,
        );
        let dropped = dropped_keys(&plan);
        assert!(dropped.is_subset(&identity_keys(&listed)), "{listed:?}");
        let missing = identity_keys(&missing);
        assert!(missing.is_disjoint(&dropped));
        assert!(!missing.is_empty());
    }

    #[test]
    fn receipt_discards_an_envelope_the_checkpoint_dropped() {
        let (store, plan) = store_with_drops("alder");
        // A node with the full history, which never trimmed.
        let untrimmed = Store::open_memory("birch").unwrap();
        untrimmed.bind_fleet(FLEET).unwrap();
        while sync(&store, "alder", &untrimmed, false).received != 0 {}
        trim(&store, &plan);
        let held_before = held(&store);
        let everything = untrimmed
            .export_replication_exchange(FLEET, &ReplicationInventory::default())
            .unwrap();
        assert!(dropped_keys(&plan).is_subset(&sent_keys(&everything)));
        let receipt = store
            .receive_replication_exchange("birch", FLEET, &everything)
            .unwrap();
        store.validate_replication_backlog().unwrap();
        store.project_replication_backlog().unwrap();
        assert_eq!(receipt.received, 0);
        assert_eq!(receipt.duplicate, everything.envelopes.len());
        assert_eq!(held(&store), held_before);
        for claim in &plan.claims {
            assert!(store.claim_by_id(&claim.id).unwrap().is_none());
        }
    }

    #[test]
    fn trimmed_untrimmed_and_full_inventory_nodes_keep_replicating() {
        let (alder, plan) = store_with_drops("alder");
        // Birch runs this build and has not trimmed. Cedar exchanges complete inventories
        // without range digests, as an older build does. Both hold the full history.
        let birch = Store::open_memory("birch").unwrap();
        let cedar = Store::open_memory("cedar").unwrap();
        for store in [&birch, &cedar] {
            store.bind_fleet(FLEET).unwrap();
            while sync(&alder, "alder", store, false).received != 0 {}
        }
        trim(&alder, &plan);
        let full_history = held(&birch);

        // Every node writes after the trim, and they exchange until nothing moves.
        write_diagnostics(&alder, "daemon/alder-after", 2);
        write_diagnostics(&birch, "daemon/birch", 2);
        write_diagnostics(&cedar, "daemon/cedar", 2);
        let nodes = [(&alder, "alder"), (&birch, "birch"), (&cedar, "cedar")];
        for _ in 0..10 {
            let mut moved = 0;
            for (from, from_name) in nodes {
                for (to, to_name) in nodes {
                    if from_name != to_name {
                        // Cedar never sends or asks with range digests.
                        let buckets = from_name != "cedar" && to_name != "cedar";
                        moved += sync(from, from_name, to, buckets).received;
                    }
                }
            }
            if moved == 0 {
                break;
            }
        }
        assert_eq!(authority(&alder), authority(&birch));
        assert_eq!(authority(&alder), authority(&cedar));
        for (store, _) in nodes {
            for subject in ["daemon/alder-after", "daemon/birch", "daemon/cedar"] {
                assert_eq!(
                    store.claims_for(subject, None).unwrap().len(),
                    2,
                    "{subject}"
                );
            }
        }
        // The trimmed node took nothing back; the others lost nothing.
        assert!(held(&alder).is_disjoint(&dropped_keys(&plan)));
        assert!(full_history.is_subset(&held(&birch)));
        assert!(full_history.is_subset(&held(&cedar)));
        for claim in &plan.claims {
            assert!(alder.claim_by_id(&claim.id).unwrap().is_none());
            assert!(birch.claim_by_id(&claim.id).unwrap().is_some());
        }
    }

    #[test]
    fn the_inventory_follows_deletions_and_later_envelopes() {
        let (store, plan) = store_with_drops("alder");
        let before = authority(&store);
        trim(&store, &plan);
        assert_eq!(authority(&store), before);
        // A later envelope extends the snapshot built after the trim incrementally.
        write_diagnostics(&store, "daemon/alder-after", 1);
        let snapshot = store.replication_snapshot().unwrap();
        let (full, _) = full_compact_replication_inventory(&store.readers.get()).unwrap();
        assert_eq!(snapshot.inventory.digest, full.digest);
        assert_ne!(snapshot.inventory.digest, before);
        let sent = sent_keys(
            &store
                .export_replication_exchange(FLEET, &ReplicationInventory::default())
                .unwrap(),
        );
        assert!(sent.is_disjoint(&dropped_keys(&plan)));
        assert_eq!(sent, held(&store));
    }

    #[test]
    fn evidence_may_cite_a_dropped_claim() {
        let (store, plan) = store_with_drops("alder");
        trim(&store, &plan);
        let dropped = &plan.claims[0].id;
        let mut citing = diagnostic("daemon/alder-after", 0, "cites a dropped claim");
        citing.evidence = vec![dropped.clone()];
        store.append_claim(&citing).unwrap();
        assert!(store.evidence_exists(dropped).unwrap());
        let mut missing = diagnostic("daemon/alder-after", 1, "cites nothing");
        missing.evidence = vec!["no-such-claim".into()];
        assert_eq!(
            store.append_claim(&missing).unwrap_err().code,
            "missing-evidence"
        );
    }

    #[test]
    fn a_retry_of_a_dropped_claim_is_answered_without_a_duplicate() {
        let (store, plan) = store_with_drops("alder");
        trim(&store, &plan);
        let (operation, _) = claim_operation(&diagnostic("daemon/alder", 0, "slow 0"))
            .unwrap()
            .unwrap();
        let dropped = plan
            .claims
            .iter()
            .find(|claim| claim.operation_id.as_deref() == Some(operation.as_str()))
            .expect("the oldest diagnostic was dropped");
        let before = store.claims_for("daemon/alder", None).unwrap().len();

        let retry = store
            .append_claim(&diagnostic("daemon/alder", 0, "slow 0"))
            .unwrap_err();
        assert_eq!(retry.code, "claim-checkpointed");
        assert_eq!(retry.details["claim_id"], json!(dropped.id));
        let different = store
            .append_claim(&diagnostic("daemon/alder", 0, "another reason"))
            .unwrap_err();
        assert_eq!(different.code, "idempotency-mismatch");
        assert_eq!(
            store.claims_for("daemon/alder", None).unwrap().len(),
            before
        );

        // The operation has no row, since no stored claim can stand for it, and a rebuild, as
        // on every open, agrees.
        assert!(
            operation_tx(&store.readers.get(), &operation)
                .unwrap()
                .is_none()
        );
        assert!(store.operation_projection_drift().unwrap().is_empty());
        store.rebuild_claim_projections().unwrap();
        assert!(
            operation_tx(&store.readers.get(), &operation)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_reused_operation_of_a_dropped_claim_still_conflicts() {
        let (alder, plan) = store_with_drops("alder");
        let birch = Store::open_memory("birch").unwrap();
        birch.bind_fleet(FLEET).unwrap();
        while sync(&alder, "alder", &birch, false).received != 0 {}
        trim(&alder, &plan);

        // Another writer that never saw the dropped claim reuses its idempotency key for a
        // different request.
        let cedar = Store::open_memory("cedar").unwrap();
        cedar.bind_fleet(FLEET).unwrap();
        cedar
            .append_claim(&diagnostic("daemon/alder", 0, "a different request"))
            .unwrap();
        while sync(&cedar, "cedar", &alder, false).received != 0 {}
        while sync(&cedar, "cedar", &birch, false).received != 0 {}

        // The node that trimmed marks the subject indeterminate, as the node that did not.
        for store in [&alder, &birch] {
            let answer = has_unknown_claim_at(&store.readers.get(), "daemon/alder", None).unwrap();
            assert!(
                answer
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("idempotency-conflict:")),
                "{answer:?}"
            );
            assert!(store.operation_projection_drift().unwrap().is_empty());
        }
        alder.rebuild_claim_projections().unwrap();
        assert!(alder.operation_projection_drift().unwrap().is_empty());
    }

    #[test]
    fn a_repeat_of_a_dropped_request_is_no_conflict() {
        let (alder, plan) = store_with_drops("alder");
        trim(&alder, &plan);
        // Another writer repeats the dropped request exactly.
        let cedar = Store::open_memory("cedar").unwrap();
        cedar.bind_fleet(FLEET).unwrap();
        cedar
            .append_claim(&diagnostic("daemon/alder", 0, "slow 0"))
            .unwrap();
        while sync(&cedar, "cedar", &alder, false).received != 0 {}
        let answer = has_unknown_claim_at(&alder.readers.get(), "daemon/alder", None).unwrap();
        assert!(
            !answer
                .as_deref()
                .is_some_and(|reason| reason.starts_with("idempotency-conflict:")),
            "{answer:?}"
        );
        assert!(alder.operation_projection_drift().unwrap().is_empty());
    }

    #[test]
    fn the_committed_index_never_moves_back() {
        let store = Store::open_memory("alder").unwrap();
        store.bind_fleet(FLEET).unwrap();
        write_diagnostics(&store, "daemon/alder", 3);
        let index = store.index().unwrap();
        let sealed = store.checkpoint_sealed_set(now_ms() + 1_000).unwrap();
        // Drop the most recently arrived claim with its envelope, which no rule would do.
        let newest = sealed
            .claims
            .iter()
            .max_by_key(|claim| claim.claim.store_index)
            .unwrap();
        assert_eq!(newest.claim.store_index, index);
        let envelope = sealed
            .envelopes
            .iter()
            .find(|envelope| envelope.key == newest.envelope)
            .unwrap();
        store
            .apply_checkpoint_drop(
                CHECKPOINT,
                &[EnvelopeTombstone {
                    writer: envelope.key.writer.clone(),
                    sequence: envelope.key.sequence,
                    envelope_hash: envelope.key.envelope_hash.clone(),
                    accepted_at_unix_ms: envelope.accepted_at_unix_ms,
                }],
                &[claim_tombstone(newest)],
            )
            .unwrap();
        assert_eq!(store.index().unwrap(), index);
        assert_eq!(current_index(&store.readers.get()).unwrap(), index);
        let next = store
            .append_claim(&diagnostic("daemon/alder-after", 0, "after"))
            .unwrap();
        assert!(next.store_index > index);
    }

    #[test]
    fn a_manifest_lists_every_tombstone_up_to_its_checkpoint_in_pages() {
        let (store, plan) = store_with_drops("alder");
        trim(&store, &plan);
        let cut = checkpoint_cut(CHECKPOINT).unwrap();
        let manifest = store.checkpoint_manifest(CHECKPOINT, cut).unwrap();
        assert_eq!(manifest.envelopes, plan.envelopes);
        assert_eq!(manifest.claims, plan.claims);
        // Pages of every size give the same manifest, across the envelope-claim boundary.
        for limit in 1..=plan.envelopes.len() + plan.claims.len() + 1 {
            assert_eq!(
                store.read_manifest(CHECKPOINT, cut, limit).unwrap(),
                manifest
            );
        }

        // Tombstones of a later checkpoint are not part of an earlier one's manifest.
        write_diagnostics(&store, "daemon/alder-later", 3);
        let later = plan_drops(&store.checkpoint_sealed_set(now_ms() + 1_000).unwrap());
        assert!(!later.envelopes.is_empty());
        store
            .apply_checkpoint_drop("checkpoint/2026-09-28", &later.envelopes, &later.claims)
            .unwrap();
        assert_eq!(
            store.checkpoint_manifest(CHECKPOINT, cut).unwrap(),
            manifest
        );
        let cumulative = store
            .checkpoint_manifest(
                "checkpoint/2026-09-28",
                checkpoint_cut("2026-09-28").unwrap(),
            )
            .unwrap();
        assert_eq!(
            cumulative.envelopes.len(),
            plan.envelopes.len() + later.envelopes.len()
        );
    }

    fn tombstones() -> CheckpointManifest {
        let cut = checkpoint_cut(CHECKPOINT).unwrap();
        let envelope = |writer: &str, sequence: u64, hash: &str| EnvelopeTombstone {
            writer: writer.into(),
            sequence,
            envelope_hash: hash.into(),
            accepted_at_unix_ms: cut - 10_000 + u128::from(sequence),
        };
        let claim = |id: &str, envelope: &EnvelopeTombstone| ClaimTombstone {
            id: id.into(),
            writer: envelope.writer.clone(),
            sequence: envelope.sequence,
            envelope_hash: envelope.envelope_hash.clone(),
            subject: "daemon/alder".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            predecessors: Vec::new(),
            operation_id: None,
            request_digest: None,
            accepted_at_unix_ms: envelope.accepted_at_unix_ms,
        };
        let first = envelope("alder", 1, "a1");
        let second = envelope("birch", 7, "b7");
        let mut rich = claim("claim-1", &first);
        rich.actor = Some("agent/alder.worker".into());
        rich.predecessors = vec!["claim-0".into(), "claim-00".into()];
        rich.operation_id = Some("op/1".into());
        rich.request_digest = Some("digest-1".into());
        let plain = claim("claim-2", &second);
        CheckpointManifest {
            checkpoint: CHECKPOINT.into(),
            cut_unix_ms: cut,
            envelopes: vec![first, second],
            claims: vec![rich, plain],
        }
    }

    #[test]
    fn a_manifest_that_differs_in_any_tombstone_field_is_rejected() {
        let manifest = tombstones();
        let digest = drop_digest(&manifest.envelopes, &manifest.claims);
        verify_checkpoint_manifest(&manifest, &digest).unwrap();

        type Change = fn(&mut CheckpointManifest);
        let changes: [(&str, Change); 25] = [
            ("subject", |m| m.claims[0].subject = "daemon/birch".into()),
            ("kind", |m| m.claims[0].kind = "observer.observed".into()),
            ("actor removed", |m| m.claims[0].actor = None),
            ("actor added", |m| {
                m.claims[1].actor = Some("person/pat".into())
            }),
            ("predecessor removed", |m| {
                m.claims[0].predecessors.pop();
            }),
            ("predecessor added", |m| {
                m.claims[1].predecessors.push("claim-1".into())
            }),
            ("predecessors reordered", |m| {
                m.claims[0].predecessors.reverse()
            }),
            ("operation changed", |m| {
                m.claims[0].operation_id = Some("op/2".into())
            }),
            ("operation removed", |m| m.claims[0].operation_id = None),
            ("operation added", |m| {
                m.claims[1].operation_id = Some("op/1".into())
            }),
            ("request digest changed", |m| {
                m.claims[0].request_digest = Some("digest-2".into())
            }),
            ("request digest removed", |m| {
                m.claims[0].request_digest = None
            }),
            ("claim accepted time", |m| {
                m.claims[0].accepted_at_unix_ms -= 1
            }),
            ("claim moved to another envelope", |m| {
                m.claims[0].writer = "birch".into();
                m.claims[0].sequence = 7;
                m.claims[0].envelope_hash = "b7".into();
            }),
            ("claim writer", |m| m.claims[0].writer = "cedar".into()),
            ("claim sequence", |m| m.claims[0].sequence = 2),
            ("claim envelope hash", |m| {
                m.claims[0].envelope_hash = "a2".into()
            }),
            ("envelope accepted time", |m| {
                m.envelopes[0].accepted_at_unix_ms += 1
            }),
            ("envelope identity", |m| {
                m.envelopes[1].envelope_hash = "b8".into();
                m.claims[1].envelope_hash = "b8".into();
            }),
            ("tombstone added", |m| {
                let mut extra = m.claims[1].clone();
                extra.id = "claim-3".into();
                m.claims.push(extra);
            }),
            ("claim tombstone removed", |m| {
                m.claims.pop();
            }),
            ("envelope tombstone removed", |m| {
                m.envelopes.pop();
                m.claims.pop();
            }),
            ("claim listed twice", |m| {
                let twice = m.claims[1].clone();
                m.claims.push(twice);
            }),
            ("envelope listed twice", |m| {
                let twice = m.envelopes[1].clone();
                m.envelopes.push(twice);
            }),
            ("claim at the cut", |m| {
                m.claims[1].accepted_at_unix_ms = m.cut_unix_ms
            }),
        ];
        for (name, change) in changes {
            let mut changed = manifest.clone();
            change(&mut changed);
            let error = verify_checkpoint_manifest(&changed, &digest)
                .expect_err(&format!("a manifest with a changed {name} passed"));
            assert!(
                matches!(
                    error.code,
                    "checkpoint-manifest-mismatch" | "checkpoint-manifest-invalid"
                ),
                "{name}: {error:?}"
            );
        }
        // A manifest whose drop differs from the one its participants verified.
        let error = verify_checkpoint_manifest(&manifest, &"0".repeat(64)).unwrap_err();
        assert_eq!(error.code, "checkpoint-manifest-mismatch");
        // A page for another checkpoint does not continue a manifest.
        let mut other = manifest.clone();
        assert!(
            other
                .append(CheckpointManifestPage {
                    checkpoint: "checkpoint/2026-09-28".into(),
                    cut_unix_ms: manifest.cut_unix_ms,
                    envelopes: Vec::new(),
                    claims: Vec::new(),
                    next: None,
                })
                .is_err()
        );
    }
}

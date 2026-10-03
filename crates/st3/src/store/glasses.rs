//! Private workspaces are derived directly from durable claims. No cache is authority.
use super::*;
use st3_schema::glasses::MAX_GLASSES;

fn glass_claims(connection: &Connection, person: &str, through: u64) -> Result<Vec<ClaimRecord>> {
    let mut statement = connection.prepare(&canonical_sql(
        "WITH ranked AS (
            SELECT claims.id, ROW_NUMBER() OVER (PARTITION BY claims.subject, claims.kind ORDER BY CANONICAL_ASC(claims)) AS first,
                ROW_NUMBER() OVER (PARTITION BY claims.subject, claims.kind ORDER BY CANONICAL_DESC(claims)) AS last
            FROM claims WHERE claims.kind IN ('glass.upserted','glass.deleted') AND claims.store_index<=?2
                AND substr(claims.subject,1,length(?1))=?1
        )
        SELECT claims.id, claims.store_index, claims.batch_id, claims.subject, claims.kind, claims.origin, claims.actor, claims.body, claims.predecessors, claims.accepted_at_unix_ms
        FROM claims JOIN ranked ON ranked.id=claims.id WHERE ranked.first=1 OR ranked.last=1
        ORDER BY CANONICAL_ASC(claims)"))?;
    Ok(statement
        .query_map(
            params![format!("glass/{person}/"), through.min(i64::MAX as u64)],
            claim_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Deleted IDs stay retired even when an offline writer sends a later edit. For quota,
/// earliest-created live IDs win the available slots. Overflow is retained, and becomes
/// visible when a slot opens. Updating a glass cannot reorder its creation priority.
fn live_claims(claims: Vec<ClaimRecord>) -> Vec<ClaimRecord> {
    let mut heads = BTreeMap::new();
    let mut created = Vec::new();
    let mut deleted = BTreeSet::new();
    for claim in claims {
        if claim.kind == "glass.deleted" {
            deleted.insert(claim.subject.clone());
        } else {
            if !heads.contains_key(&claim.subject) {
                created.push(claim.subject.clone());
            }
            heads.insert(claim.subject.clone(), claim);
        }
    }
    created
        .into_iter()
        .filter(|id| !deleted.contains(id))
        .take(MAX_GLASSES)
        .filter_map(|id| heads.remove(&id))
        .collect()
}

fn resource(claim: &ClaimRecord) -> Result<Value> {
    let body = st3_schema::glasses::body_for_read(&claim.body["fields"]["body"])?;
    Ok(
        json!({"id":claim.subject, "kind":"glass", "revision":claim.id,
        "body":body, "deleted":false,
        "base_revision":claim.body["fields"]["base_revision"],
        "replaced_revision":claim.body["fields"]["replaced_revision"],
        "updated_at":chrono::DateTime::from_timestamp_millis(i64::try_from(claim.accepted_at_unix_ms).unwrap_or(i64::MAX)).unwrap_or(chrono::DateTime::UNIX_EPOCH).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)}),
    )
}

pub(super) fn glasses_at(
    connection: &Connection,
    person: &str,
    through: u64,
) -> Result<Vec<Value>> {
    let claims = match glass_heads::current_claims(connection, person, through)? {
        Some(claims) => claims,
        None => live_claims(glass_claims(connection, person, through)?),
    };
    let mut result: Vec<_> = claims
        .iter()
        .map(resource)
        .collect::<Result<_>>()?;
    result.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(result)
}
impl Store {
    pub(crate) fn glasses_changed(&self, after: u64, through: u64) -> Result<bool> {
        Ok(self.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM claims WHERE store_index>?1 AND store_index<=?2 AND kind IN ('glass.upserted','glass.deleted'))", params![after, through], |row| row.get(0))?)
    }
    pub fn glasses(&self, person: &str, through: u64) -> Result<Vec<Value>> {
        let connection = self.readers.get();
        glasses_at(&connection, person, through)
    }
}

/// Called under the claim writer's transaction, after idempotency has been resolved.
/// Never validate quotas at replication admission: that would depend on arrival order.
pub(super) fn prepare(
    transaction: &Transaction<'_>,
    input: &ClaimInput,
) -> Result<Option<BTreeMap<String, Value>>, St3Error> {
    if !input.subject.starts_with("glass/") {
        return Ok(None);
    }
    let person =
        st3_schema::glasses::owner(&input.subject).map_err(|e| St3Error::new(e.code, e.message))?;
    glass_heads::flush(transaction).map_err(internal)?;
    let head = glass_heads::head(transaction, &input.subject).map_err(internal)?;
    if head.as_ref().is_some_and(|head| head.deleted) {
        return Err(St3Error::new(
            "glass-deleted",
            "a deleted glass ID cannot be reused",
        ));
    }
    let head = head.as_ref().and_then(|head| head.claim_id.as_deref());
    if input.kind == "glass.deleted" && head.is_none() {
        return Err(St3Error::new("not-found", "the glass does not exist"));
    }
    if input.kind == "glass.upserted" && head.is_none() {
        if !input.fields.get("base_revision").is_none_or(Value::is_null) {
            return Err(St3Error::new(
                "invalid-glass-base",
                "creation requires a null base_revision",
            ));
        }
        if glass_heads::live_count(transaction, person).map_err(internal)? >= MAX_GLASSES {
            return Err(St3Error::new(
                "glass-limit",
                "at most 100 glasses may be live",
            ));
        }
    }
    let mut fields = input.fields.clone();
    fields.entry("base_revision".into()).or_insert(Value::Null);
    fields.insert(
        "replaced_revision".into(),
        head.map_or(Value::Null, |id| json!(id)),
    );
    Ok(Some(fields))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{exchange_from, exchange_of, receive_and_project};
    use super::*;
    use crate::model::ReplicationInventory;

    fn input(id: usize, name: &str) -> ClaimInput {
        ClaimInput { subject:format!("glass/person/ada/019a0000-0000-7000-8000-{id:012x}"), kind:"glass.upserted".into(), actor:Some("person/ada".into()),
            fields:serde_json::from_value(json!({"body":{"name":name,"layout":{"tabs":[{"pane":"opaque:anything"}]}},"base_revision":null})).unwrap(), evidence:vec![], expected_subject:None, idempotency_key:None }
    }
    fn sync(source: &Store, target: &Store) {
        let exchange = exchange_from(source, &ReplicationInventory::default());
        receive_and_project(target, &source.origin, &exchange);
    }

    fn history_oracle(store: &Store) -> Vec<Value> {
        let connection = store.readers.get();
        let mut resources: Vec<_> =
            live_claims(glass_claims(&connection, "person/ada", u64::MAX).unwrap())
                .iter()
                .map(|claim| resource(claim).unwrap())
                .collect();
        resources.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        resources
    }

    #[test]
    fn historical_glass_snapshot_survives_many_newer_heads() {
        let store = Store::open_memory("alder").unwrap();
        store.set_write_clock_at(1_800_000_000_000).unwrap();
        let first = store.append_claim(&input(1, "Original")).unwrap();
        let snapshot = store.glasses("person/ada", first.store_index).unwrap();
        let mut previous = first.clone();
        for edit in 1..=128 {
            store.set_write_clock_at(1_800_000_000_000 + edit).unwrap();
            let mut update = input(1, &format!("Edit {edit}"));
            update.fields.insert("base_revision".into(), json!(previous.id));
            let next = store.append_claim(&update).unwrap();
            assert_eq!(next.body["fields"]["replaced_revision"], previous.id);
            previous = next;
        }
        let latest = store.glasses("person/ada", u64::MAX).unwrap();
        assert_eq!(latest[0]["revision"], previous.id);
        assert_eq!(latest[0]["body"]["name"], "Edit 128");
        assert_eq!(latest, history_oracle(&store));
        assert_eq!(store.glasses("person/ada", first.store_index).unwrap(), snapshot);
        assert_eq!(snapshot[0]["body"]["name"], "Original");
        store.rebuild_claim_projections().unwrap();
        assert_eq!(store.glasses("person/ada", first.store_index).unwrap(), snapshot);
        assert_eq!(store.glasses("person/ada", u64::MAX).unwrap(), latest);
    }

    #[test]
    fn canonical_glass_head_survives_reverse_duplicates_replay_and_reopen() {
        let alder = Store::open_memory("alder").unwrap();
        let birch = Store::open_memory("birch").unwrap();
        alder.set_write_clock_at(1_800_000_000_000).unwrap();
        birch.set_write_clock_at(1_800_000_000_000).unwrap();
        let earlier = alder.append_claim(&input(1, "On alder")).unwrap();
        let canonical = birch.append_claim(&input(1, "On birch")).unwrap();
        let mut envelopes = exchange_from(&alder, &ReplicationInventory::default()).envelopes;
        envelopes.extend(exchange_from(&birch, &ReplicationInventory::default()).envelopes);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claims.sqlite");
        let forward = Store::open_memory("cedar").unwrap();
        let reverse = Store::open(&path, "elm").unwrap();
        for envelope in &envelopes {
            receive_and_project(&forward, "relay", &exchange_of("relay", vec![envelope.clone()]));
        }
        for envelope in envelopes.iter().rev().chain(envelopes.iter().rev()) {
            receive_and_project(&reverse, "relay", &exchange_of("relay", vec![envelope.clone()]));
        }
        let expected = history_oracle(&forward);
        assert_eq!(expected[0]["revision"], canonical.id);
        assert_eq!(expected[0]["body"]["name"], "On birch");
        assert_eq!(forward.glasses("person/ada", u64::MAX).unwrap(), expected);
        assert_eq!(reverse.glasses("person/ada", u64::MAX).unwrap(), expected);
        assert_eq!(history_oracle(&reverse), expected);
        reverse.rebuild_claim_projections().unwrap();
        assert_eq!(reverse.glasses("person/ada", u64::MAX).unwrap(), expected);
        drop(reverse);
        let reverse = Store::open(&path, "elm").unwrap();
        assert_eq!(reverse.glasses("person/ada", u64::MAX).unwrap(), expected);
        reverse.set_write_clock_at(1_800_000_000_001).unwrap();
        let mut stale = input(1, "After reopen");
        stale.fields.insert("base_revision".into(), json!(earlier.id));
        let next = reverse.append_claim(&stale).unwrap();
        assert_eq!(next.body["fields"]["base_revision"], earlier.id);
        assert_eq!(next.body["fields"]["replaced_revision"], canonical.id);
        let current = reverse.glasses("person/ada", u64::MAX).unwrap();
        assert_eq!(current[0]["revision"], next.id);
        assert_eq!(current, history_oracle(&reverse));
    }

    #[test]
    fn dirty_canonical_record_correction_reads_and_prepares_the_correct_head() {
        let store = Store::open_memory("alder").unwrap();
        store.set_write_clock_at(1_800_000_000_000).unwrap();
        let first = store.append_claim(&input(1, "First wire position")).unwrap();
        let second = {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            let second = append_claim_record_tx(
                &transaction,
                "alder",
                &first.subject,
                "glass.upserted",
                Some("person/ada"),
                &json!({"fields": input(1, "Second wire position").fields}),
                &[],
                Some(&first.batch_id),
            )
            .unwrap();
            for (claim, position) in [(&first, 0), (&second, 1)] {
                transaction.execute(
                    "INSERT INTO replica_records(record_ref, writer, sequence, envelope_hash,
                        position, raw, state, claim_id, updated_at_unix_ms)
                     VALUES (?1, 'alder', 1, 'fixture', ?2, X'', 'valid', ?3, '0')",
                    params![format!("record/{}", claim.id), position, claim.id],
                ).unwrap();
            }
            super::super::glass_heads::flush(&transaction).unwrap();
            transaction.commit().unwrap();
            second
        };
        assert_eq!(store.glasses("person/ada", u64::MAX).unwrap()[0]["revision"], second.id);
        store.connection.write().execute(
            "UPDATE replica_records SET position=2 WHERE claim_id=?1", [&first.id],
        ).unwrap();
        // The durable metadata changed, but no projection worker has flushed it yet.
        let corrected = store.glasses("person/ada", u64::MAX).unwrap();
        assert_eq!(corrected[0]["revision"], first.id);
        assert_eq!(corrected[0]["body"]["name"], "First wire position");
        assert_eq!(corrected, history_oracle(&store));
        store.set_write_clock_at(1_800_000_000_001).unwrap();
        let mut update = input(1, "After correction");
        update.fields.insert("base_revision".into(), json!(second.id));
        let next = store.append_claim(&update).unwrap();
        assert_eq!(next.body["fields"]["replaced_revision"], first.id);
        assert_eq!(store.glasses("person/ada", u64::MAX).unwrap()[0]["revision"], next.id);
        assert_eq!(store.glasses("person/ada", u64::MAX).unwrap(), history_oracle(&store));
    }
    #[test]
    fn split_ratios_survive_replication_and_projection_replay() {
        let a = Store::open_memory("alder").unwrap();
        let b = Store::open_memory("birch").unwrap();
        let mut claim = input(1, "Main");
        let body = json!({"name":"Main","layout":{"split":"right","ratio":0.3,"children":[{"tabs":[]},{"split":"below","ratio":0.9,"children":[{"tabs":[]},{"tabs":[]}]}]}});
        claim.fields.insert("body".into(), body.clone());
        a.append_claim(&claim).unwrap();
        sync(&a, &b);
        assert_eq!(
            a.glasses("person/ada", u64::MAX).unwrap(),
            b.glasses("person/ada", u64::MAX).unwrap()
        );
        b.rebuild_claim_projections().unwrap();
        assert_eq!(b.glasses("person/ada", u64::MAX).unwrap()[0]["body"], body);
    }
    #[test]
    fn legacy_glasses_survive_reopen_replication_and_replacement_with_their_revision() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claims.sqlite");
        let a = Store::open(&path, "alder").unwrap();
        let mut legacy = input(1, "Main");
        let old = json!({"name":"Main","tabs":[{"title":"Work","layout":{
            "split":"right","children":[{"pane":"opaque:a"},{"pane":"opaque:b"}]
        }}]});
        legacy.fields.insert("body".into(), old.clone());
        let original = a.append_claim(&legacy).unwrap();
        drop(a);
        let a = Store::open(&path, "alder").unwrap();
        let b = Store::open_memory("birch").unwrap();
        sync(&a, &b);
        let live = a.glasses("person/ada", u64::MAX).unwrap();
        assert_eq!(live, b.glasses("person/ada", u64::MAX).unwrap());
        assert_eq!(live[0]["revision"], original.id);
        assert_eq!(
            live[0]["body"],
            json!({"name":"Main","layout":{"tabs":[
                {"title":"Work","pane":"opaque:a"},{"pane":"opaque:b"}
            ]}})
        );
        a.rebuild_claim_projections().unwrap();
        assert_eq!(live, a.glasses("person/ada", u64::MAX).unwrap());
        let mut replace = input(1, "Renamed");
        replace
            .fields
            .insert("body".into(), live[0]["body"].clone());
        replace
            .fields
            .insert("base_revision".into(), json!(original.id));
        let replaced = a.append_claim(&replace).unwrap();
        assert_eq!(replaced.body["fields"]["replaced_revision"], original.id);
        assert_eq!(
            a.claims_for(&original.subject, None).unwrap()[0].body["fields"]["body"],
            old
        );
        sync(&a, &b);
        assert_eq!(
            a.glasses("person/ada", u64::MAX).unwrap(),
            b.glasses("person/ada", u64::MAX).unwrap()
        );
    }
    #[test]
    fn glass_stale_revisions_idempotency_and_retirement() {
        let a = Store::open_memory("alder").unwrap();
        let b = Store::open_memory("birch").unwrap();
        let first = a.append_claim(&input(1, "Main")).unwrap();
        sync(&a, &b);
        let mut update = input(1, "Renamed");
        update
            .fields
            .insert("base_revision".into(), json!(first.id));
        update.idempotency_key = Some("rename-glass".into());
        let renamed = a.append_claim(&update).unwrap();
        assert_eq!(renamed.body["fields"]["replaced_revision"], first.id);
        let mut stale = input(1, "Stale write");
        stale.fields.insert("base_revision".into(), json!(first.id));
        let stale = a.append_claim(&stale).unwrap();
        assert_eq!(stale.body["fields"]["base_revision"], first.id);
        assert_eq!(stale.body["fields"]["replaced_revision"], renamed.id);
        assert_eq!(a.append_claim(&update).unwrap().id, renamed.id);
        let mut delete = input(1, "");
        delete.kind = "glass.deleted".into();
        delete.fields.remove("body");
        a.append_claim(&delete).unwrap();
        assert_eq!(
            a.append_claim(&input(1, "Reuse")).unwrap_err().code,
            "glass-deleted"
        );
        b.append_claim(&input(1, "Offline edit")).unwrap();
        sync(&a, &b);
        sync(&b, &a);
        assert!(a.glasses("person/ada", i64::MAX as u64).unwrap().is_empty());
        assert_eq!(
            a.glasses("person/ada", i64::MAX as u64).unwrap(),
            b.glasses("person/ada", i64::MAX as u64).unwrap()
        );
        a.rebuild_claim_projections().unwrap();
        assert!(a.glasses("person/ada", i64::MAX as u64).unwrap().is_empty());
        let scratch = tempfile::tempdir().unwrap();
        let (plan, proof) = a.plan_checkpoint(now_ms() + 1, scratch.path()).unwrap();
        assert!(proof.passed);
        assert!(
            plan.claims
                .iter()
                .all(|claim| !claim.kind.starts_with("glass."))
        );
    }
    #[test]
    fn glass_concurrent_creates_converge_at_quota_in_any_arrival_order() {
        let a = Store::open_memory("alder").unwrap();
        let b = Store::open_memory("birch").unwrap();
        a.set_write_clock_at(1_800_000_000_000).unwrap();
        b.set_write_clock_at(1_800_000_000_000).unwrap();
        for id in 0..99 {
            a.append_claim(&input(id, "Workspace")).unwrap();
        }
        sync(&a, &b);
        a.append_claim(&input(99, "On alder")).unwrap();
        b.append_claim(&input(100, "On birch")).unwrap();
        a.set_write_clock_at(1_800_000_000_001).unwrap();
        let mut edited = None;
        for edit in 0..32 {
            edited = Some(a.append_claim(&input(0, &format!("Edited {edit}"))).unwrap());
        }
        assert_eq!(
            a.append_claim(&input(101, "Over quota")).unwrap_err().code,
            "glass-limit"
        );
        let mut envelopes = exchange_from(&a, &ReplicationInventory::default()).envelopes;
        envelopes.extend(exchange_from(&b, &ReplicationInventory::default()).envelopes);
        let c = Store::open_memory("cedar").unwrap();
        let d = Store::open_memory("elm").unwrap();
        for e in &envelopes {
            receive_and_project(&c, "relay", &exchange_of("relay", vec![e.clone()]));
        }
        for e in envelopes.iter().rev() {
            receive_and_project(&d, "relay", &exchange_of("relay", vec![e.clone()]));
        }
        let live = c.glasses("person/ada", i64::MAX as u64).unwrap();
        assert_eq!(live.len(), 100);
        assert_eq!(live, d.glasses("person/ada", i64::MAX as u64).unwrap());
        assert_eq!(live[0]["revision"], edited.unwrap().id);
        assert!(live.iter().any(|glass| glass["id"] == input(99, "").subject));
        assert!(!live.iter().any(|glass| glass["id"] == input(100, "").subject));
        assert_eq!(live, history_oracle(&c));
        c.rebuild_claim_projections().unwrap();
        assert_eq!(live, c.glasses("person/ada", i64::MAX as u64).unwrap());
        let mut delete = input(0, "");
        delete.kind = "glass.deleted".into();
        delete.fields.remove("body");
        c.set_write_clock_at(1_800_000_000_002).unwrap();
        c.append_claim(&delete).unwrap();
        sync(&c, &d);
        assert_eq!(c.glasses("person/ada", i64::MAX as u64).unwrap().len(), 100);
        assert!(c.glasses("person/ada", u64::MAX).unwrap().iter()
            .any(|glass| glass["id"] == input(100, "").subject));
        assert_eq!(
            c.glasses("person/ada", i64::MAX as u64).unwrap(),
            d.glasses("person/ada", i64::MAX as u64).unwrap()
        );
    }
    #[test]
    fn glass_owner_admission_and_generic_projections_are_private() {
        let store = Store::open_memory("alder").unwrap();
        for actor in [None, Some("agent/worker"), Some("person/alex")] {
            let mut claim = input(1, "Private");
            claim.actor = actor.map(str::to_owned);
            assert_eq!(
                store.append_claim(&claim).unwrap_err().code,
                "glass-owner-forbidden"
            );
        }
        assert_eq!(
            store
                .append_client_claim(&input(1, "Bypass"))
                .unwrap_err()
                .code,
            "claim-write-forbidden"
        );
        let first = store.append_claim(&input(1, "Private")).unwrap();
        assert!(
            store
                .claims_page(None, None, 0, None, false, 100)
                .unwrap()
                .claims
                .is_empty()
        );
        assert!(
            store
                .status_history(Some(&first.subject), None, None)
                .unwrap()
                .subjects
                .is_empty()
        );
        assert!(
            store
                .glasses("person/alex", i64::MAX as u64)
                .unwrap()
                .is_empty()
        );
        let connection = store.readers.get();
        let batch=connection.query_row("SELECT id, origin, replica_sequence, previous_hash, hash, accepted_at_unix_ms FROM batches WHERE id=?1",[&first.batch_id],|r| Ok(crate::model::ReplicaBatch { id:r.get(0)?,origin:r.get(1)?,replica_sequence:r.get(2)?,previous_hash:r.get(3)?,hash:r.get(4)?,accepted_at_unix_ms:r.get::<_,String>(5)?.parse().unwrap(), claims:vec![] })).unwrap();
        let mut forged = first.clone();
        forged.actor = Some("agent/worker".into());
        forged.id = claim_hash(
            &forged.batch_id,
            &forged.subject,
            &forged.kind,
            &forged.origin,
            forged.actor.as_deref(),
            &forged.body,
            &forged.predecessors,
        )
        .unwrap();
        assert_eq!(
            validate_replicated_claim(&connection, &batch, &forged)
                .err()
                .unwrap()
                .code,
            "glass-owner-forbidden"
        );
    }
}

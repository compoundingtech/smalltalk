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

fn resource(claim: &ClaimRecord) -> Value {
    json!({"id":claim.subject, "kind":"glass", "revision":claim.id,
        "body":claim.body["fields"]["body"], "deleted":false,
        "base_revision":claim.body["fields"]["base_revision"],
        "replaced_revision":claim.body["fields"]["replaced_revision"],
        "updated_at":chrono::DateTime::from_timestamp_millis(i64::try_from(claim.accepted_at_unix_ms).unwrap_or(i64::MAX)).unwrap_or(chrono::DateTime::UNIX_EPOCH).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)})
}

pub(super) fn glasses_at(
    connection: &Connection,
    person: &str,
    through: u64,
) -> Result<Vec<Value>> {
    let mut result: Vec<_> = live_claims(glass_claims(connection, person, through)?)
        .iter()
        .map(resource)
        .collect();
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
    let claims = glass_claims(transaction, person, u64::MAX).map_err(internal)?;
    let history: Vec<_> = claims
        .iter()
        .filter(|c| c.subject == input.subject)
        .collect();
    if history.iter().any(|c| c.kind == "glass.deleted") {
        return Err(St3Error::new(
            "glass-deleted",
            "a deleted glass ID cannot be reused",
        ));
    }
    let head = history.last();
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
        if live_claims(claims.clone()).len() >= MAX_GLASSES {
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
        head.map_or(Value::Null, |c| json!(c.id)),
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
            fields:serde_json::from_value(json!({"body":{"name":name,"tabs":[{"layout":{"pane":"home:"}}]},"base_revision":null})).unwrap(), evidence:vec![], expected_subject:None, idempotency_key:None }
    }
    fn sync(source: &Store, target: &Store) {
        let exchange = exchange_from(source, &ReplicationInventory::default());
        receive_and_project(target, &source.origin, &exchange);
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
        for id in 0..99 {
            a.append_claim(&input(id, "Workspace")).unwrap();
        }
        sync(&a, &b);
        a.append_claim(&input(99, "On alder")).unwrap();
        b.append_claim(&input(100, "On birch")).unwrap();
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
        c.rebuild_claim_projections().unwrap();
        assert_eq!(live, c.glasses("person/ada", i64::MAX as u64).unwrap());
        let mut delete = input(0, "");
        delete.kind = "glass.deleted".into();
        delete.fields.remove("body");
        c.append_claim(&delete).unwrap();
        sync(&c, &d);
        assert_eq!(c.glasses("person/ada", i64::MAX as u64).unwrap().len(), 100);
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

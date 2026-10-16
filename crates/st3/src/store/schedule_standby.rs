//! Standby adoption reads existing membership and own-origin startup advertisements only.
//! No storage, replication writer, retention or primary-only admission change.
use super::*;

const WRITER_TABLES: [(&str, &str); 3] = [
    ("replica_envelopes", "writer"), ("batches", "origin"), ("checkpoint_envelopes", "writer"),
];
fn writer_seek(table: &str, column: &str) -> String {
    format!("SELECT {column} FROM {table} WHERE {column}>?1 ORDER BY {column} LIMIT 1")
}

fn refused(reason: impl Into<String>) -> St3Error {
    St3Error::new("schedule-standby-unavailable", reason)
}

pub(super) fn validate_publication(
    connection: &Connection,
    origin: &str,
    intent: &NormalizedIntent,
) -> Result<(), St3Error> {
    for desired in intent.subjects.values().filter(|desired| desired.kind == "schedule") {
        let Some(spec) = crate::graph::schedule_spec(&desired.desired, origin) else { continue };
        let Some(standby) = spec.standby.as_ref() else { continue };
        let membership = fleet_membership_tx(connection).map_err(internal)?;
        // Syntax may be stored before membership is known. It is never eligible there.
        if membership.anchor().is_none() { continue; }
        if !matches!(membership.state(&standby.host), crate::fleet::MemberState::Current(_)) {
            return Err(refused("standby member unknown, never eligible"));
        }
        adoption_epoch(connection, origin, &spec)?;
    }
    Ok(())
}

fn adoption_epoch(connection: &Connection, origin: &str, spec: &crate::model::ScheduleSpec) -> Result<String, St3Error> {
    let Some(standby) = spec.standby.as_ref() else { return Err(refused("standby is not declared")) };
    let membership = fleet_membership_tx(connection).map_err(internal)?;
    if membership.anchor().is_none()
        || !matches!(membership.state(origin), crate::fleet::MemberState::Current(_)) {
        return Err(refused("standby member unknown, never eligible; anchored current membership is unknown"));
    }
    for host in [&spec.host, &standby.host] {
        if !matches!(membership.state(host), crate::fleet::MemberState::Current(_)) {
            return Err(refused("standby member unknown, never eligible"));
        }
    }
    // A membership mutation not yet signed must not enable against yesterday's members.
    let pending: bool = connection.query_row(&format!(
        "SELECT EXISTS(SELECT 1 FROM claims c JOIN batches b ON b.id=c.batch_id
         LEFT JOIN replica_envelopes e ON e.batch_id=b.id
         WHERE c.origin=?1 AND c.kind IN ({})
           AND NOT EXISTS(SELECT 1 FROM replica_envelope_signatures s
             WHERE s.writer=b.origin AND s.sequence=b.replica_sequence
               AND s.envelope_hash=e.envelope_hash))",
        smallclaims::store::FLEET_CLAIM_KINDS), [origin], |row| row.get(0)).map_err(internal)?;
    if pending { return Err(refused("fleet membership is not settled and signed")); }
    let mut epochs = Vec::new();
    for member in membership.incarnations().filter(|member| member.end.is_none()) {
        if !matches!(membership.state(&member.name), crate::fleet::MemberState::Current(_)) {
            return Err(refused("fleet membership is ambiguous"));
        }
        let advertisement: Option<(String, Option<bool>)> = connection.query_row(&canonical_sql(
            "SELECT claims.id, json_extract(claims.body,'$.fields.features.schedule_standby')=1
             FROM claims JOIN batches ON batches.id=claims.batch_id
             WHERE claims.subject=?1 AND claims.kind='daemon.started' AND claims.origin=?2
               AND batches.replica_sequence>=?3 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            params![format!("daemon/{}", member.name), member.name, member.start],
            |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(internal)?;
        let Some((claim, Some(true))) = advertisement else {
            return Err(refused(format!("host/{} has not advertised schedule_standby=1; mixed fleets remain primary-only", member.name)));
        };
        epochs.push((member.name.clone(), member.start, claim));
    }
    let fenced = membership.legacy_removed_names().collect::<BTreeSet<_>>();
    for (table, column) in WRITER_TABLES {
        let mut previous = String::new();
        let mut statement = connection.prepare(&writer_seek(table, column)).map_err(internal)?;
        while let Some(writer) = statement.query_row([&previous], |row| row.get::<_, String>(0)).optional().map_err(internal)? {
            if !membership.is_keyed_writer(&writer) && !fenced.contains(&writer) {
                return Err(refused("unfenced legacy writer prevents standby adoption"));
            }
            previous = writer;
        }
    }
    canonical_hash(&(spec.host.as_str(), standby, epochs)).map_err(internal)
}

impl Store {
    pub(crate) fn schedule_standby_adoption_epoch(&self, spec: &crate::model::ScheduleSpec) -> Result<String, St3Error> {
        adoption_epoch(&self.readers.get(), &self.origin, spec)
    }

    pub(crate) fn schedule_standby_history_complete(&self) -> Result<bool> {
        // Neither an unknown record nor an admitted-but-unprojected receipt proves absence.
        if self.replication_projection_deferred() || self.replication_catching_up()
            || self.first_sync()?.is_some_and(|sync| sync.state == "syncing") {
            return Ok(false);
        }
        let unknown: bool = self.readers.get().query_row(
            "SELECT EXISTS(SELECT 1 FROM replica_records WHERE state='unknown')", [], |row| row.get(0))?;
        Ok(!unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::StatementStatus;

    // Synthetic retained-row reader cost controls, NOT authenticated admission evidence.
    #[test]
    fn standby_legacy_writer_seeks_use_prefix_indexes_not_claim_volume() {
        let store = Store::open_memory("jade").unwrap();
        let mut connection = store.connection.write();
        let tx = connection.transaction().unwrap();
        let mut before = Vec::new();
        for upper in [8, 4_000] {
            for index in if upper == 8 { 0..8 } else { 8..4_000 } {
                let id = format!("fixture-{index}");
                tx.execute("INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms) VALUES(?1,'legacy',?2,?1,'1')", params![id,index+1]).unwrap();
                tx.execute("INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,batch_id,relay,receipt_state,received_at_unix_ms) VALUES('legacy',?1,?2,'1',X'00',?2,'legacy','pending','1')", params![index+1,id]).unwrap();
                tx.execute("INSERT INTO checkpoint_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,checkpoint) VALUES('legacy',?1,?2,1,'checkpoint/fixture')", params![index+1,id]).unwrap();
            }
            let mut measured = Vec::new();
            for (table, column) in WRITER_TABLES {
                let sql = writer_seek(table, column);
                let plan = tx.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap()
                    .query_map([""], |row| row.get::<_,String>(3)).unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>().unwrap().join("; ");
                assert!(plan.contains("SEARCH") && !plan.contains("SCAN ") && !plan.contains("TEMP B-TREE"), "{table}: {plan}");
                let mut statement = tx.prepare(&sql).unwrap();
                statement.reset_status(StatementStatus::VmStep);
                assert_eq!(statement.query_row([""], |row| row.get::<_,String>(0)).unwrap(), "legacy");
                let _ = statement.query_row(["legacy"], |row| row.get::<_,String>(0)).optional().unwrap();
                measured.push(statement.get_status(StatementStatus::VmStep));
            }
            if upper == 8 { before = measured; } else {
                for (small, large) in before.iter().zip(measured) {
                    assert!(large <= small + 16, "writer seek grew with payload volume: {small} -> {large}");
                }
            }
        }
    }
}

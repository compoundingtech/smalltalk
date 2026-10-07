//! Pinned-anchor, one admission per member, direct removal slice. Root floor is zero and the
//! root never ends. General via/sponsor chains, writer windows, conflicts and cycles are absent.
//! Signers come from verified envelope storage (or the configured local node key), never JSON.
use super::real_views::{SCHEMA, fields, put, row, text};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;
use smallclaims::{
    ClaimRecord,
    ivm::{Definition, View},
};
use std::collections::BTreeSet;

pub struct AnchorFleet {
    pub root: String,
    pub anchor: String,
    pub local_origin: String,
    pub local_signer: Option<String>,
    pub fingerprint: &'static str,
}
impl AnchorFleet {
    fn signed(&self, tx: &Transaction<'_>, claim: &ClaimRecord) -> Result<bool> {
        let (writer, sequence): (String, u64) = tx.query_row(
            "SELECT origin,replica_sequence FROM batches WHERE id=?1",
            [&claim.batch_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            writer == claim.origin,
            "claim origin differs from its batch writer"
        );
        let envelope: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM replica_envelopes WHERE batch_id=?1)",
            [&claim.batch_id],
            |r| r.get(0),
        )?;
        let verified: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM replica_envelopes e
            JOIN replica_envelope_signatures s ON s.writer=e.writer AND s.sequence=e.sequence
                AND s.envelope_hash=e.envelope_hash
            WHERE e.batch_id=?1 AND s.member_key=?2)",
            params![claim.batch_id, self.anchor],
            |r| r.get(0),
        )?;
        Ok(writer == self.root
            && sequence >= 1
            && (verified
                || (!envelope
                    && writer == self.local_origin
                    && self.local_signer.as_deref() == Some(&self.anchor))))
    }
}
impl View for AnchorFleet {
    fn definition(&self) -> Definition {
        Definition {
            name: "fleet-membership",
            fingerprint: self.fingerprint,
            kinds: &[
                "fleet.member-admitted",
                "fleet.member-removed",
                "fleet.member-left",
                "fleet.member-endpoints",
            ],
            local_kinds: &[],
            max_contributions: 2,
        }
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        c.execute_batch(SCHEMA)?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS app_fleet_facts(id TEXT PRIMARY KEY,subject TEXT NOT NULL,
            member_key TEXT NOT NULL,kind TEXT NOT NULL,high_water INTEGER,payload TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS app_fleet_admissions ON app_fleet_facts(subject,kind,member_key,id);
            CREATE INDEX IF NOT EXISTS app_fleet_removals ON app_fleet_facts(subject,member_key,kind,high_water DESC,id);")?;
        c.execute_batch("CREATE TABLE IF NOT EXISTS app_fleet_signers(writer TEXT NOT NULL,sequence INTEGER NOT NULL,
            member_key TEXT NOT NULL,PRIMARY KEY(writer,sequence,member_key));
            CREATE TRIGGER IF NOT EXISTS app_fleet_signature_insert AFTER INSERT ON replica_envelope_signatures
            WHEN NOT EXISTS(SELECT 1 FROM app_fleet_signers WHERE writer=NEW.writer AND sequence=NEW.sequence AND member_key=NEW.member_key)
            BEGIN
                UPDATE ivm_views SET ready=0 WHERE name='fleet-membership' AND ready<>0 AND EXISTS(
                    SELECT 1 FROM batches b INDEXED BY batches_writer_sequence
                    CROSS JOIN claims c INDEXED BY claims_batch_index
                    CROSS JOIN app_fleet_facts f
                    WHERE b.origin=NEW.writer AND b.replica_sequence=NEW.sequence AND c.batch_id=b.id AND f.id=c.id);
            END;
            CREATE TRIGGER IF NOT EXISTS app_fleet_signature_delete AFTER DELETE ON replica_envelope_signatures
            BEGIN
                UPDATE ivm_views SET ready=0 WHERE name='fleet-membership' AND ready<>0 AND EXISTS(
                    SELECT 1 FROM batches b INDEXED BY batches_writer_sequence
                    CROSS JOIN claims c INDEXED BY claims_batch_index CROSS JOIN app_fleet_facts f
                    WHERE b.origin=OLD.writer AND b.replica_sequence=OLD.sequence AND c.batch_id=b.id AND f.id=c.id);
            END;
            CREATE TRIGGER IF NOT EXISTS app_fleet_signature_update AFTER UPDATE ON replica_envelope_signatures
            WHEN OLD.writer<>NEW.writer OR OLD.sequence<>NEW.sequence OR OLD.envelope_hash<>NEW.envelope_hash
                OR OLD.member_key<>NEW.member_key OR OLD.signature<>NEW.signature
            BEGIN
                UPDATE ivm_views SET ready=0 WHERE name='fleet-membership' AND ready<>0 AND EXISTS(
                    SELECT 1 FROM batches b INDEXED BY batches_writer_sequence
                    CROSS JOIN claims c INDEXED BY claims_batch_index CROSS JOIN app_fleet_facts f
                    WHERE ((b.origin=OLD.writer AND b.replica_sequence=OLD.sequence)
                      OR (b.origin=NEW.writer AND b.replica_sequence=NEW.sequence)) AND c.batch_id=b.id AND f.id=c.id);
            END;")?;
        Ok(())
    }
    fn affected_keys(
        &self,
        tx: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<&ClaimRecord>,
    ) -> Result<BTreeSet<String>> {
        let mut keys = BTreeSet::new();
        if let Some(old) = old {
            tx.execute("DELETE FROM app_fleet_facts WHERE id=?1", [&old.id])?;
            keys.insert(old.subject.clone());
        }
        if let Some(new) = new {
            let f = fields(new);
            if !self.signed(tx, new)? {
                anyhow::bail!(
                    "membership signer/authority is unproven; claims-only fixture admission is not authority admission"
                );
            }
            ensure!(
                matches!(
                    new.kind.as_str(),
                    "fleet.member-admitted" | "fleet.member-removed"
                ),
                "endpoints/leave require the writer-window closure operator"
            );
            let member_key = text(f, "member_key")?;
            if new.subject == format!("host/{}", self.root) {
                ensure!(
                    new.kind == "fleet.member-admitted"
                        && f["via"] == "anchor"
                        && member_key == self.anchor
                        && (f.get("writer_floor").is_none()
                            || f["writer_floor"].as_u64() == Some(0))
                        && f["mode"] == "listening",
                    "anchor window changes require the authority closure operator"
                );
            } else {
                ensure!(
                    row(tx, "fleet-membership", &format!("host/{}", self.root))?.is_some(),
                    "anchor admission is missing; bounded closure bootstrap not installed"
                );
                if new.kind == "fleet.member-admitted" {
                    ensure!(
                        f["via"] == "member"
                            && f["sponsor"] == self.root
                            && f["writer_floor"].as_u64() == Some(0)
                            && f["mode"] == "listening",
                        "non-direct sponsor/mode/window requires the authority closure operator"
                    );
                } else {
                    ensure!(
                        f["removed_by"] == self.root,
                        "non-anchor removal requires the authority closure operator"
                    );
                }
            }
            let high_water = if new.kind == "fleet.member-removed" {
                Some(
                    f["high_water"]
                        .as_u64()
                        .context("removal high water missing")?,
                )
            } else {
                None
            };
            tx.execute(
                "INSERT INTO app_fleet_facts VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO NOTHING",
                params![
                    new.id,
                    new.subject,
                    member_key,
                    new.kind,
                    high_water,
                    f.to_string()
                ],
            )?;
            let (writer, sequence): (String, u64) = tx.query_row(
                "SELECT origin,replica_sequence FROM batches WHERE id=?1",
                [&new.batch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            // A configured local node key is already counted on unsealed local claims. Its
            // eventual verified signature is equal evidence; another key is an invalidation.
            tx.execute(
                "INSERT OR IGNORE INTO app_fleet_signers VALUES(?1,?2,?3)",
                params![writer, sequence, self.anchor],
            )?;
            keys.insert(new.subject.clone());
        }
        Ok(keys)
    }
    fn maintain_key(
        &self,
        tx: &Transaction<'_>,
        subject: &str,
        _: Option<&ClaimRecord>,
        _: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        let admissions = tx
            .prepare_cached(
                "SELECT member_key,id,payload FROM app_fleet_facts
            WHERE subject=?1 AND kind='fleet.member-admitted' ORDER BY member_key,id LIMIT 2",
            )?
            .query_map([subject], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            admissions.len() <= 1,
            "multiple admissions require incarnation/conflict closure"
        );
        let next = if let Some((member, id, payload)) = admissions.first() {
            let f: serde_json::Value = serde_json::from_str(payload)?;
            let end: Option<u64> = tx
                .query_row(
                    "SELECT high_water FROM app_fleet_facts
                WHERE subject=?1 AND member_key=?2 AND kind='fleet.member-removed'
                ORDER BY high_water DESC,id LIMIT 1",
                    params![subject, member],
                    |r| r.get(0),
                )
                .optional()?;
            Some(json!({"subject":subject,"member_key":member,"admitted":id,
                "start":f["writer_floor"].as_u64().unwrap_or(0).saturating_add(1),
                "end":end,"state":if end.is_some(){"ended"}else{"current"}}))
        } else {
            None
        };
        Ok(Some(put(tx, "fleet-membership", subject, "fleet", next)?))
    }
}

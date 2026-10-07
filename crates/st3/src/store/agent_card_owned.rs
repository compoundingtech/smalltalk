//! Namespace-scoped owned membership dependency for the complete agent-card operator.
//!
//! Inputs are captured ClaimRecords with exact canonical keys and the owned_sets per-kind
//! eligibility decision (no repaired original). No apply/read queries production history.
//! Extraction is explicit bounded Installer paging owned by the source adapter; this module
//! neither certifies source coverage nor publishes Ready. Limits fail rather than truncate.
//! Captured input bytes are capped separately from derived table size; candidate retention
//! is bounded by the input quota, not claimed constant under unchanged answers. Source-owner
//! journal/candidate/disk and commit-inclusive writer measurements remain integration gates.
//! Read rows and changes are internal processing evidence, never authority or public events.
use super::*;
use anyhow::ensure;
use serde::Deserialize;
use smallclaims::ivm::install::Namespace;

/// Include this dependency/exhaustion contract in the owning operator/source fingerprints.
pub(super) const FINGERPRINT: &str = "owned-membership.v2;relevant-domain;receipt-sequence-revision-claim;canonical-stops;repair-eligibility;historical-counted-dependencies;members128;sets64;lineage64;claim256k;claims100k;captured64m";

const MAX_MEMBERS: usize = 128;
const MAX_SETS: usize = 64;
const MAX_LINEAGE: usize = 64;
const MAX_CLAIM_BYTES: usize = 256 * 1024;
const MAX_CLAIMS: i64 = 100_000;
const MAX_BYTES: i64 = 64 * 1024 * 1024;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_owned_state(namespace TEXT PRIMARY KEY,claims INTEGER NOT NULL,bytes INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS local_agent_owned_claims(namespace TEXT,claim TEXT,subject TEXT,kind TEXT,eligible INTEGER,body TEXT,rank BLOB,staged INTEGER,PRIMARY KEY(namespace,claim));
CREATE TABLE IF NOT EXISTS local_agent_owned_receipts(namespace TEXT,set_id TEXT,claim TEXT,sequence BLOB,revision TEXT,reference TEXT,repository TEXT,source_ref TEXT,body TEXT,PRIMARY KEY(namespace,claim));
CREATE INDEX IF NOT EXISTS local_agent_owned_winner ON local_agent_owned_receipts(namespace,set_id,sequence DESC,revision DESC,claim DESC);
CREATE INDEX IF NOT EXISTS local_agent_owned_reference ON local_agent_owned_receipts(namespace,reference,claim);
CREATE TABLE IF NOT EXISTS local_agent_owned_members(namespace TEXT,set_id TEXT,claim TEXT,subject TEXT,sequence BLOB,revision TEXT,retired INTEGER,body TEXT,PRIMARY KEY(namespace,claim,subject,retired));
CREATE INDEX IF NOT EXISTS local_agent_owned_known ON local_agent_owned_members(namespace,set_id,subject,sequence DESC,revision DESC,claim DESC,retired DESC);
CREATE TABLE IF NOT EXISTS local_agent_owned_owners(namespace TEXT,subject TEXT,set_id TEXT,count INTEGER NOT NULL,PRIMARY KEY(namespace,subject,set_id));
CREATE INDEX IF NOT EXISTS local_agent_owned_set_subjects ON local_agent_owned_owners(namespace,set_id,subject);
CREATE TABLE IF NOT EXISTS local_agent_owned_bindings(namespace TEXT,set_id TEXT,repository TEXT,source_ref TEXT,count INTEGER NOT NULL,PRIMARY KEY(namespace,set_id,repository,source_ref));
CREATE TABLE IF NOT EXISTS local_agent_owned_sequences(namespace TEXT,set_id TEXT,sequence BLOB,revision TEXT,count INTEGER NOT NULL,PRIMARY KEY(namespace,set_id,sequence,revision));
CREATE TABLE IF NOT EXISTS local_agent_owned_dependencies(namespace TEXT,claim TEXT,set_id TEXT,kind TEXT,key TEXT,PRIMARY KEY(namespace,claim,kind,key));
CREATE TABLE IF NOT EXISTS local_agent_owned_reverse_refs(namespace TEXT,kind TEXT,key TEXT,set_id TEXT,count INTEGER NOT NULL,PRIMARY KEY(namespace,kind,key,set_id));
CREATE TABLE IF NOT EXISTS local_agent_owned_stops(namespace TEXT,subject TEXT,set_id TEXT,predecessor TEXT,origin TEXT,claim TEXT,rank BLOB,PRIMARY KEY(namespace,claim));
CREATE INDEX IF NOT EXISTS local_agent_owned_stop_winner ON local_agent_owned_stops(namespace,subject,set_id,predecessor,origin,rank DESC);
CREATE TABLE IF NOT EXISTS local_agent_owned_staged(namespace TEXT,subject TEXT,count INTEGER NOT NULL,PRIMARY KEY(namespace,subject));
CREATE TABLE IF NOT EXISTS local_agent_owned_selected(namespace TEXT,set_id TEXT,body TEXT,PRIMARY KEY(namespace,set_id));
CREATE TABLE IF NOT EXISTS local_agent_owned_rows(namespace TEXT,subject TEXT,body TEXT,generation INTEGER NOT NULL,PRIMARY KEY(namespace,subject));
"#;

pub(super) fn create_schema(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    Ok(())
}

/// Static owned-domain dispatch. The staged predicate mirrors the legacy all-kind non-null
/// root owned_set check. Also dispatch old/new indexed references regardless of kind; when
/// admitting a receipt, resolve its already-captured member facts by claim ID from the source
/// adapter, including other-kind facts, before declaring coverage. Never consult live history.
pub(super) fn relevant(claim: &ClaimRecord) -> bool {
    matches!(
        claim.kind.as_str(),
        "owned-set.revised" | "intent.desired" | "mission.published"
    ) || claim
        .body
        .get("owned_set")
        .is_some_and(|value| !value.is_null())
}
pub(super) fn references(db: &Connection, ns: &Namespace, id: &str) -> Result<bool> {
    referenced_at(db, ns.as_str(), id)
}
fn referenced_at(db: &Connection, ns: &str, id: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_owned_reverse_refs WHERE namespace=?1 AND kind='claim' AND key=?2)",params![ns,id],|row|row.get(0))?)
}

/// The adapter supplies authoritative eligibility, including rank-only edits without arrivals.
pub(super) struct Captured<'a> {
    pub claim: &'a ClaimRecord,
    pub rank: &'a canonical::ClaimKey,
    pub eligible: bool,
}

#[derive(Default, Debug)]
pub(super) struct Changes {
    /// Internal dependency subjects; filter to public agent IDs before event publication.
    pub affected: BTreeSet<String>,
    pub changed: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct SelectedMember {
    pub set: String,
    pub receipt: String,
    pub receipt_revision: String,
    pub updated_at: String,
    pub source: owned_sets::Source,
    pub policy: Option<crate::rollout::Policy>,
    pub actor: Option<String>,
    pub member: owned_sets::Member,
    pub retired: bool,
    pub implicit: bool,
    pub manual: bool,
    pub desired: Option<DesiredSubject>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct OwnedRow {
    pub subject: String,
    pub owners: Vec<String>,
    /// Nonempty means no selected membership/rollout/effect may be used.
    pub blockers: Vec<String>,
    pub selected: Option<SelectedMember>,
}

impl OwnedRow {
    pub(super) fn rollout(&self) -> Result<Option<crate::rollout::Selection>> {
        ensure!(self.blockers.is_empty(), "owned membership pending");
        let Some(selected) = &self.selected else {
            return Ok(None);
        };
        let Some(desired) = &selected.desired else {
            return Ok(None);
        };
        let policy = match selected.policy.clone() {
            Some(policy) => policy,
            None if selected.manual => crate::rollout::Policy::when_idle(30 * 60 * 1000, false),
            None => return Ok(None),
        };
        Ok(Some(crate::rollout::Selection {
            set: selected.set.clone(),
            receipt: selected.receipt.clone(),
            source: selected.source.clone(),
            policy,
            manual: selected.manual,
            desired_token: selected.member.claim.clone(),
            target: crate::rollout::target(desired)?,
            desired: desired.clone(),
            actor: selected.actor.clone(),
        }))
    }
}

pub(super) fn read(db: &Connection, ns: &Namespace, subject: &str) -> Result<Option<OwnedRow>> {
    read_at(db, ns.as_str(), subject)
}
fn read_at(db: &Connection, ns: &str, subject: &str) -> Result<Option<OwnedRow>> {
    db.query_row(
        "SELECT body FROM local_agent_owned_rows WHERE namespace=?1 AND subject=?2",
        params![ns, subject],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()?
    .flatten()
    .map(|s| Ok(serde_json::from_str(&s)?))
    .transpose()
}

/// Savepoint rollback includes candidates/dependencies/output/generations. A caller preserving
/// raw admission on a logical limit must fence its Installer source and public Views afterwards.
pub(super) fn apply_claim(
    tx: &Transaction<'_>,
    ns: &Namespace,
    old: Option<&ClaimRecord>,
    new: Option<Captured<'_>>,
) -> Result<Changes> {
    apply(tx, ns.as_str(), old, new)
}
fn apply(
    tx: &Transaction<'_>,
    ns: &str,
    old: Option<&ClaimRecord>,
    new: Option<Captured<'_>>,
) -> Result<Changes> {
    let ids: BTreeSet<_> = old
        .into_iter()
        .map(|claim| claim.id.as_str())
        .chain(new.as_ref().map(|fact| fact.claim.id.as_str()))
        .collect();
    let keep_new = match &new {
        Some(fact) => relevant(fact.claim) || referenced_at(tx, ns, &fact.claim.id)?,
        None => false,
    };
    let mut stored = false;
    for id in &ids {
        stored |= tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_agent_owned_claims WHERE namespace=?1 AND claim=?2)",
            params![ns, id],
            |row| row.get::<_, bool>(0),
        )?;
    }
    if !stored && !keep_new {
        return Ok(Changes::default());
    }
    tx.execute_batch("SAVEPOINT agent_owned_apply")?;
    let result = edit(tx, ns, &ids, new.filter(|_| keep_new));
    match result {
        Ok(changes) => {
            tx.execute_batch("RELEASE agent_owned_apply")?;
            Ok(changes)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO agent_owned_apply; RELEASE agent_owned_apply")?;
            Err(error)
        }
    }
}

fn strings(db: &Connection, sql: &str, ns: &str, key: &str, limit: usize) -> Result<Vec<String>> {
    let mut query = db.prepare(sql)?;
    let rows = query
        .query_map(params![ns, key, limit as u64 + 1], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    ensure!(rows.len() <= limit, "owned dependency fanout exhausted");
    Ok(rows)
}
fn subjects(db: &Connection, ns: &str, set: &str) -> Result<Vec<String>> {
    strings(
        db,
        "SELECT subject FROM local_agent_owned_owners WHERE namespace=?1 AND set_id=?2 ORDER BY subject LIMIT ?3",
        ns,
        set,
        MAX_MEMBERS,
    )
}
fn owners(db: &Connection, ns: &str, subject: &str) -> Result<Vec<String>> {
    strings(
        db,
        "SELECT set_id FROM local_agent_owned_owners WHERE namespace=?1 AND subject=?2 ORDER BY set_id LIMIT ?3",
        ns,
        subject,
        MAX_SETS,
    )
}
fn dependencies(db: &Connection, ns: &str, kind: &str, key: &str) -> Result<Vec<String>> {
    let mut q=db.prepare("SELECT set_id FROM local_agent_owned_reverse_refs WHERE namespace=?1 AND kind=?2 AND key=?3 ORDER BY set_id LIMIT ?4")?;
    let sets = q
        .query_map(params![ns, kind, key, MAX_SETS as u64 + 1], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    ensure!(sets.len() <= MAX_SETS, "owned dependency fanout exhausted");
    Ok(sets)
}
fn captured(db: &Connection, ns: &str, id: &str) -> Result<Option<ClaimRecord>> {
    db.query_row(
        "SELECT body FROM local_agent_owned_claims WHERE namespace=?1 AND claim=?2 AND eligible=1",
        params![ns, id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| Ok(serde_json::from_str(&s)?))
    .transpose()
}
fn parsed_receipt(c: &ClaimRecord) -> Result<Option<owned_sets::View>> {
    if c.kind != "owned-set.revised" {
        return Ok(None);
    }
    let Ok(receipt) =
        serde_json::from_value::<owned_sets::Revision>(c.body["fields"]["body"].clone())
    else {
        return Ok(None);
    };
    ensure!(
        receipt.members.len() + receipt.retired.len() <= MAX_MEMBERS,
        "owned receipt member bound exhausted"
    );
    let revision = canonical_hash(&receipt)?;
    if c.body["fields"]["revision"].as_str() != Some(&revision) {
        return Ok(None);
    }
    let accepted: i64 = c.accepted_at_unix_ms.try_into()?;
    let updated_at = chrono::DateTime::from_timestamp_millis(accepted)
        .context("invalid receipt time")?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(Some(owned_sets::View {
        kind: "owned-set".into(),
        id: c.subject.clone(),
        revision,
        claim: c.id.clone(),
        updated_at,
        receipt,
        blockers: vec![],
    }))
}
fn receipt(db: &Connection, ns: &str, id: &str) -> Result<Option<owned_sets::View>> {
    db.query_row(
        "SELECT body FROM local_agent_owned_receipts WHERE namespace=?1 AND claim=?2",
        params![ns, id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| Ok(serde_json::from_str(&s)?))
    .transpose()
}

fn edit(
    tx: &Transaction<'_>,
    ns: &str,
    ids: &BTreeSet<&str>,
    new: Option<Captured<'_>>,
) -> Result<Changes> {
    let mut sets = BTreeSet::new();
    let mut affected = BTreeSet::new();
    for id in ids {
        sets.extend(dependencies(tx, ns, "claim", id)?);
        if let Some(v) = receipt(tx, ns, id)? {
            sets.insert(v.id.clone());
            sets.extend(dependencies(
                tx,
                ns,
                "previous",
                &format!("{}@{}", v.id, v.revision),
            )?);
            for subject in v.receipt.members.keys().chain(v.receipt.retired.keys()) {
                affected.insert(subject.clone());
                sets.extend(owners(tx, ns, subject)?);
            }
        }
        if let Some(c) = tx
            .query_row(
                "SELECT body FROM local_agent_owned_claims WHERE namespace=?1 AND claim=?2",
                params![ns, id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            let c: ClaimRecord = serde_json::from_str(&c)?;
            affected.insert(c.subject.clone());
            if c.kind == "intent.desired" {
                sets.extend(owners(tx, ns, &c.subject)?);
            }
        }
    }
    let next_body = new
        .as_ref()
        .map(|c| serde_json::to_string(c.claim))
        .transpose()?;
    ensure!(
        next_body
            .as_ref()
            .is_none_or(|body| body.len() <= MAX_CLAIM_BYTES),
        "owned claim byte bound exhausted"
    );
    let next_receipt = new
        .as_ref()
        .filter(|c| c.eligible)
        .map(|c| parsed_receipt(c.claim))
        .transpose()?
        .flatten();
    if let Some(v) = &next_receipt {
        sets.insert(v.id.clone());
        sets.extend(dependencies(
            tx,
            ns,
            "previous",
            &format!("{}@{}", v.id, v.revision),
        )?);
        for subject in v.receipt.members.keys().chain(v.receipt.retired.keys()) {
            affected.insert(subject.clone());
            sets.extend(owners(tx, ns, subject)?);
        }
    }
    if let Some(c) = &new {
        sets.extend(dependencies(tx, ns, "claim", &c.claim.id)?);
        if c.claim.kind == "intent.desired" {
            sets.extend(owners(tx, ns, &c.claim.subject)?);
        }
        affected.insert(c.claim.subject.clone());
    }
    ensure!(sets.len() <= MAX_SETS, "owned affected set bound exhausted");
    for set in &sets {
        affected.extend(subjects(tx, ns, set)?);
    }
    for id in ids {
        retract(tx, ns, id)?;
    }
    if let Some(c) = new {
        let body = next_body.context("missing captured claim body")?;
        let staged = c.claim.body.get("owned_set").is_some_and(|v| !v.is_null());
        tx.execute("INSERT INTO local_agent_owned_state VALUES(?1,1,?2) ON CONFLICT(namespace) DO UPDATE SET claims=claims+1,bytes=bytes+excluded.bytes",params![ns,body.len() as i64])?;
        let counts: (i64, i64) = tx.query_row(
            "SELECT claims,bytes FROM local_agent_owned_state WHERE namespace=?1",
            [ns],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            counts.0 <= MAX_CLAIMS && counts.1 <= MAX_BYTES,
            "owned captured input quota exhausted"
        );
        tx.execute(
            "INSERT INTO local_agent_owned_claims VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                ns,
                c.claim.id,
                c.claim.subject,
                c.claim.kind,
                c.eligible,
                body,
                canonical::sortable_key(c.rank),
                staged
            ],
        )?;
        if staged {
            tx.execute("INSERT INTO local_agent_owned_staged VALUES(?1,?2,1) ON CONFLICT(namespace,subject) DO UPDATE SET count=count+1",params![ns,c.claim.subject])?;
        }
        if let Some(v) = next_receipt {
            admit_receipt(tx, ns, &v)?;
        }
        if c.eligible
            && c.claim.kind == "intent.desired"
            && c.claim.actor.as_deref() == Some("daemon/runtime")
            && c.claim.body["kind"] == "stop"
            && c.claim.predecessors.len() == 1
        {
            if let Some(set) = c.claim.body["owned_set"].as_str() {
                tx.execute(
                    "INSERT INTO local_agent_owned_stops VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        ns,
                        c.claim.subject,
                        set,
                        c.claim.predecessors[0],
                        c.claim.origin,
                        c.claim.id,
                        canonical::sortable_key(c.rank)
                    ],
                )?;
            }
        }
    }
    for set in &sets {
        affected.extend(subjects(tx, ns, set)?);
        maintain_set(tx, ns, set)?;
    }
    ensure!(
        affected.len() <= MAX_SETS * MAX_MEMBERS,
        "owned affected subject bound exhausted"
    );
    let mut changes = Changes {
        affected,
        changed: BTreeSet::new(),
    };
    for subject in &changes.affected {
        if maintain_row(tx, ns, subject)? {
            changes.changed.insert(subject.clone());
        }
    }
    Ok(changes)
}

fn retract(tx: &Transaction<'_>, ns: &str, id: &str) -> Result<()> {
    if let Some(v) = receipt(tx, ns, id)? {
        for subject in v.receipt.members.keys().chain(v.receipt.retired.keys()) {
            tx.execute("UPDATE local_agent_owned_owners SET count=count-1 WHERE namespace=?1 AND subject=?2 AND set_id=?3",params![ns,subject,v.id])?;
            tx.execute("DELETE FROM local_agent_owned_owners WHERE namespace=?1 AND subject=?2 AND set_id=?3 AND count=0",params![ns,subject,v.id])?;
        }
        let seq = v.receipt.source.sequence.to_be_bytes();
        tx.execute("UPDATE local_agent_owned_bindings SET count=count-1 WHERE namespace=?1 AND set_id=?2 AND repository=?3 AND source_ref=?4",params![ns,v.id,v.receipt.source.repository,v.receipt.source.r#ref])?;
        tx.execute("DELETE FROM local_agent_owned_bindings WHERE namespace=?1 AND set_id=?2 AND repository=?3 AND source_ref=?4 AND count=0",params![ns,v.id,v.receipt.source.repository,v.receipt.source.r#ref])?;
        tx.execute("UPDATE local_agent_owned_sequences SET count=count-1 WHERE namespace=?1 AND set_id=?2 AND sequence=?3 AND revision=?4",params![ns,v.id,seq.as_slice(),v.revision])?;
        tx.execute("DELETE FROM local_agent_owned_sequences WHERE namespace=?1 AND set_id=?2 AND sequence=?3 AND revision=?4 AND count=0",params![ns,v.id,seq.as_slice(),v.revision])?;
    }
    if let Some((body,subject,staged))=tx.query_row("SELECT body,subject,staged FROM local_agent_owned_claims WHERE namespace=?1 AND claim=?2",params![ns,id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?))).optional()? {
        tx.execute("UPDATE local_agent_owned_state SET claims=claims-1,bytes=bytes-?2 WHERE namespace=?1",params![ns,body.len() as i64])?;
        if staged {
            tx.execute("UPDATE local_agent_owned_staged SET count=count-1 WHERE namespace=?1 AND subject=?2",params![ns,subject])?;
            tx.execute("DELETE FROM local_agent_owned_staged WHERE namespace=?1 AND subject=?2 AND count=0",params![ns,subject])?;
        }
    }
    let refs = {
        let mut q=tx.prepare("SELECT kind,key,set_id FROM local_agent_owned_dependencies WHERE namespace=?1 AND claim=?2 LIMIT ?3")?;
        q.query_map(params![ns, id, (MAX_MEMBERS + 2) as u64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    ensure!(
        refs.len() <= MAX_MEMBERS + 1,
        "owned stored dependency bound exhausted"
    );
    for (kind, key, set) in refs {
        tx.execute("UPDATE local_agent_owned_reverse_refs SET count=count-1 WHERE namespace=?1 AND kind=?2 AND key=?3 AND set_id=?4",params![ns,kind,key,set])?;
        tx.execute("DELETE FROM local_agent_owned_reverse_refs WHERE namespace=?1 AND kind=?2 AND key=?3 AND set_id=?4 AND count=0",params![ns,kind,key,set])?;
    }
    // Fixed table names; every deletion is namespace/key scoped, never source history.
    tx.execute(
        "DELETE FROM local_agent_owned_claims WHERE namespace=?1 AND claim=?2",
        params![ns, id],
    )?;
    tx.execute(
        "DELETE FROM local_agent_owned_receipts WHERE namespace=?1 AND claim=?2",
        params![ns, id],
    )?;
    tx.execute(
        "DELETE FROM local_agent_owned_members WHERE namespace=?1 AND claim=?2",
        params![ns, id],
    )?;
    tx.execute(
        "DELETE FROM local_agent_owned_dependencies WHERE namespace=?1 AND claim=?2",
        params![ns, id],
    )?;
    tx.execute(
        "DELETE FROM local_agent_owned_stops WHERE namespace=?1 AND claim=?2",
        params![ns, id],
    )?;
    Ok(())
}

fn dependency(
    tx: &Transaction<'_>,
    ns: &str,
    v: &owned_sets::View,
    kind: &str,
    key: &str,
) -> Result<()> {
    if tx.execute(
        "INSERT OR IGNORE INTO local_agent_owned_dependencies VALUES(?1,?2,?3,?4,?5)",
        params![ns, v.claim, v.id, kind, key],
    )? != 0
    {
        tx.execute("INSERT INTO local_agent_owned_reverse_refs VALUES(?1,?2,?3,?4,1) ON CONFLICT(namespace,kind,key,set_id) DO UPDATE SET count=count+1",params![ns,kind,key,v.id])?;
    }
    Ok(())
}
fn admit_receipt(tx: &Transaction<'_>, ns: &str, v: &owned_sets::View) -> Result<()> {
    let seq = v.receipt.source.sequence.to_be_bytes();
    tx.execute(
        "INSERT INTO local_agent_owned_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            ns,
            v.id,
            v.claim,
            seq.as_slice(),
            v.revision,
            format!("{}@{}", v.id, v.revision),
            v.receipt.source.repository,
            v.receipt.source.r#ref,
            serde_json::to_string(v)?
        ],
    )?;
    tx.execute("INSERT INTO local_agent_owned_bindings VALUES(?1,?2,?3,?4,1) ON CONFLICT(namespace,set_id,repository,source_ref) DO UPDATE SET count=count+1",params![ns,v.id,v.receipt.source.repository,v.receipt.source.r#ref])?;
    tx.execute("INSERT INTO local_agent_owned_sequences VALUES(?1,?2,?3,?4,1) ON CONFLICT(namespace,set_id,sequence,revision) DO UPDATE SET count=count+1",params![ns,v.id,seq.as_slice(),v.revision])?;
    if let Some(previous) = &v.receipt.previous {
        dependency(tx, ns, v, "previous", previous)?;
    }
    for (retired, map) in [(false, &v.receipt.members), (true, &v.receipt.retired)] {
        for (subject, member) in map {
            tx.execute(
                "INSERT INTO local_agent_owned_members VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    ns,
                    v.id,
                    v.claim,
                    subject,
                    seq.as_slice(),
                    v.revision,
                    retired,
                    serde_json::to_string(member)?
                ],
            )?;
            tx.execute("INSERT INTO local_agent_owned_owners VALUES(?1,?2,?3,1) ON CONFLICT(namespace,subject,set_id) DO UPDATE SET count=count+1",params![ns,subject,v.id])?;
            dependency(tx, ns, v, "claim", &member.claim)?;
        }
    }
    Ok(())
}

fn winner(db: &Connection, ns: &str, set: &str) -> Result<Option<owned_sets::View>> {
    db.query_row("SELECT body FROM local_agent_owned_receipts WHERE namespace=?1 AND set_id=?2 ORDER BY sequence DESC,revision DESC,claim DESC LIMIT 1",params![ns,set],|r|r.get::<_,String>(0)).optional()?.map(|s|Ok(serde_json::from_str(&s)?)).transpose()
}
fn maintain_set(tx: &Transaction<'_>, ns: &str, set: &str) -> Result<()> {
    let Some(mut view) = winner(tx, ns, set)? else {
        tx.execute(
            "DELETE FROM local_agent_owned_selected WHERE namespace=?1 AND set_id=?2",
            params![ns, set],
        )?;
        return Ok(());
    };
    let mut cursor = view.clone();
    let mut seen = BTreeSet::new();
    while let Some(previous) = &cursor.receipt.previous {
        if !seen.insert(previous.clone()) {
            view.blockers.push("cyclic previous revision".into());
            break;
        }
        ensure!(seen.len() <= MAX_LINEAGE, "owned lineage bound exhausted");
        let parent=tx.query_row("SELECT body FROM local_agent_owned_receipts WHERE namespace=?1 AND reference=?2 ORDER BY claim DESC LIMIT 1",params![ns,previous],|r|r.get::<_,String>(0)).optional()?;
        let Some(parent) = parent else {
            view.blockers
                .push(format!("missing previous revision {previous}"));
            break;
        };
        let parent: owned_sets::View = serde_json::from_str(&parent)?;
        if parent.id != view.id
            || parent.receipt.source.repository != cursor.receipt.source.repository
            || parent.receipt.source.r#ref != cursor.receipt.source.r#ref
            || parent.receipt.source.sequence >= cursor.receipt.source.sequence
        {
            view.blockers.push("invalid source lineage".into());
            break;
        }
        cursor = parent;
    }
    let seq = view.receipt.source.sequence.to_be_bytes();
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_owned_sequences WHERE namespace=?1 AND set_id=?2 AND sequence=?3 AND revision<>?4)",params![ns,set,seq.as_slice(),view.revision],|r|r.get::<_,bool>(0))? { view.blockers.push(format!("conflicting source sequence {}",view.receipt.source.sequence)); }
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM local_agent_owned_bindings WHERE namespace=?1 AND set_id=?2 AND (repository<>?3 OR source_ref<>?4))",params![ns,set,view.receipt.source.repository,view.receipt.source.r#ref],|r|r.get::<_,bool>(0))? { view.blockers.push("conflicting source binding".into()); }
    for (subject, member) in view.receipt.members.iter().chain(&view.receipt.retired) {
        if owners(tx, ns, subject)?.len() > 1 {
            view.blockers.push(format!("ownership conflict: {subject}"));
        }
        let Some(claim) = captured(tx, ns, &member.claim)? else {
            view.blockers
                .push(format!("missing member reference {}", member.claim));
            continue;
        };
        let retired = view.receipt.retired.contains_key(subject);
        let valid = if member.kind == "mission" {
            claim.kind == "mission.published"
                && claim.body["revision"].as_str() == Some(&member.revision)
                && (!retired || claim.body["state"] == "retired")
        } else {
            claim.kind == "intent.desired"
                && serde_json::from_value::<DesiredSubject>(claim.body.clone()).is_ok_and(|d| {
                    desired_revision(&d) == member.revision
                        && d.subject == *subject
                        && d.owner_run.is_none()
                        && d.owner_step.is_none()
                        && if retired && member.kind == "schedule" {
                            d.kind == "schedule"
                                && crate::graph::schedule_spec(&d.desired, "unused")
                                    .is_some_and(|spec| spec.stopped)
                        } else {
                            d.kind == if retired { "stop" } else { &member.kind }
                        }
                })
        };
        if claim.subject != *subject || !valid {
            view.blockers
                .push(format!("invalid member reference {subject}"));
        }
    }
    view.blockers.sort();
    view.blockers.dedup();
    tx.execute("INSERT INTO local_agent_owned_selected VALUES(?1,?2,?3) ON CONFLICT(namespace,set_id) DO UPDATE SET body=excluded.body",params![ns,set,serde_json::to_string(&view)?])?;
    Ok(())
}
fn stop(subject: &str) -> Result<DesiredSubject> {
    let source = if let Some(name) = subject.strip_prefix("schedule/") {
        format!(
            "version 2\nschedule {} {{ stop }}\n",
            serde_json::to_string(name)?
        )
    } else {
        format!("version 2\nstop {}\n", serde_json::to_string(subject)?)
    };
    crate::graph::parse_internal_intent(&source, "unused")
        .map_err(anyhow::Error::new)?
        .subjects
        .remove(subject)
        .context("invalid retirement subject")
}
fn manual(db: &Connection, ns: &str, member: &owned_sets::Member) -> Result<bool> {
    Ok(member.manual_rollout
        || captured(db, ns, &member.claim)?
            .and_then(|c| serde_json::from_value::<DesiredSubject>(c.body).ok())
            .is_some_and(|d| crate::rollout::manual(&d)))
}
fn effective(
    db: &Connection,
    ns: &str,
    view: &owned_sets::View,
    subject: &str,
) -> Result<Option<(owned_sets::Member, bool, bool)>> {
    if let Some(member) = view.receipt.retired.get(subject) {
        return Ok(Some((member.clone(), true, false)));
    }
    if let Some(member) = view.receipt.members.get(subject) {
        if member.one_shot {
            if let Some(declaration) = captured(db, ns, &member.claim)? {
                let desired: DesiredSubject = serde_json::from_value(declaration.body)?;
                if let Some(launch) = desired.member.filter(|m| m.one_shot) {
                    let retirement=db.query_row("SELECT claim FROM local_agent_owned_stops WHERE namespace=?1 AND subject=?2 AND set_id=?3 AND predecessor=?4 AND origin=?5 ORDER BY rank DESC LIMIT 1",params![ns,subject,view.id,member.claim,launch.host],|r|r.get::<_,String>(0)).optional()?;
                    if let Some(claim) = retirement {
                        return Ok(Some((
                            owned_sets::Member {
                                claim,
                                revision: desired_revision(&stop(subject)?),
                                ..member.clone()
                            },
                            true,
                            false,
                        )));
                    }
                }
            }
        }
        return Ok(Some((member.clone(), false, false)));
    }
    let known=db.query_row("SELECT body FROM local_agent_owned_members WHERE namespace=?1 AND set_id=?2 AND subject=?3 ORDER BY sequence DESC,revision DESC,claim DESC,retired DESC LIMIT 1",params![ns,view.id,subject],|r|r.get::<_,String>(0)).optional()?;
    let Some(known) = known else { return Ok(None) };
    let known: owned_sets::Member = serde_json::from_str(&known)?;
    let member = if known.kind == "mission" {
        known
    } else {
        owned_sets::Member {
            manual_rollout: manual(db, ns, &known)?,
            one_shot: false,
            revision: desired_revision(&stop(subject)?),
            claim: view.claim.clone(),
            kind: known.kind,
        }
    };
    Ok(Some((member, true, true)))
}
fn maintain_row(tx: &Transaction<'_>, ns: &str, subject: &str) -> Result<bool> {
    let owners = owners(tx, ns, subject)?;
    let mut row = OwnedRow {
        subject: subject.into(),
        owners: owners.clone(),
        blockers: vec![],
        selected: None,
    };
    if owners.len() > 1 {
        row.blockers
            .push(format!("{subject} has conflicting set owners"));
    } else if let Some(set) = owners.first() {
        let value: String = tx.query_row(
            "SELECT body FROM local_agent_owned_selected WHERE namespace=?1 AND set_id=?2",
            params![ns, set],
            |r| r.get(0),
        )?;
        let view: owned_sets::View = serde_json::from_str(&value)?;
        row.blockers = view.blockers.clone();
        if row.blockers.is_empty() {
            if let Some((member, retired, implicit)) = effective(tx, ns, &view, subject)? {
                let desired = if member.kind == "mission" {
                    None
                } else if implicit {
                    Some(stop(subject)?)
                } else {
                    Some(serde_json::from_value(
                        captured(tx, ns, &member.claim)?
                            .context("missing effective member")?
                            .body,
                    )?)
                };
                let is_manual = match &desired {
                    Some(d) if d.kind != "stop" => crate::rollout::manual(d),
                    _ => manual(tx, ns, &member)?,
                };
                let actor = captured(tx, ns, &view.claim)?.and_then(|c| c.actor);
                row.selected = Some(SelectedMember {
                    set: set.clone(),
                    receipt: view.claim,
                    receipt_revision: view.revision,
                    updated_at: view.updated_at,
                    source: view.receipt.source,
                    policy: view.receipt.rollout,
                    actor,
                    member,
                    retired,
                    implicit,
                    manual: is_manual,
                    desired,
                });
            }
        }
    } else if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_agent_owned_staged WHERE namespace=?1 AND subject=?2)",
        params![ns, subject],
        |r| r.get::<_, bool>(0),
    )? {
        row.blockers
            .push("staged member awaits its owning set revision".into());
    }
    let after = if row.owners.is_empty() && row.blockers.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&row)?)
    };
    let before = tx
        .query_row(
            "SELECT body FROM local_agent_owned_rows WHERE namespace=?1 AND subject=?2",
            params![ns, subject],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if after == before {
        return Ok(false);
    }
    tx.execute("INSERT INTO local_agent_owned_rows VALUES(?1,?2,?3,1) ON CONFLICT(namespace,subject) DO UPDATE SET body=excluded.body,generation=generation+1",params![ns,subject,after])?;
    Ok(true)
}

/// Called only for an inactive Installer namespace. Deletes at most `limit` rows in one
/// table per call; the source owner must never reclaim a published/active namespace.
pub(super) fn reclaim(tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<bool> {
    reclaim_at(tx, ns.as_str(), limit)
}
fn reclaim_at(tx: &Transaction<'_>, ns: &str, limit: usize) -> Result<bool> {
    ensure!(
        (1..=128).contains(&limit),
        "owned reclaim page bound invalid"
    );
    let pages = [
        "DELETE FROM local_agent_owned_claims WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_claims WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_receipts WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_receipts WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_members WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_members WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_owners WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_owners WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_bindings WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_bindings WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_sequences WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_sequences WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_dependencies WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_dependencies WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_reverse_refs WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_reverse_refs WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_stops WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_stops WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_staged WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_staged WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_selected WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_selected WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_rows WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_rows WHERE namespace=?1 LIMIT ?2)",
        "DELETE FROM local_agent_owned_state WHERE namespace=?1 AND rowid IN (SELECT rowid FROM local_agent_owned_state WHERE namespace=?1 LIMIT ?2)",
    ];
    for page in pages {
        if tx.execute(page, params![ns, limit as u64])? != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_intent;

    fn bundle(command: &str, meadow: bool) -> NormalizedIntent {
        parse_intent(
            &format!(
                "version 2\nagent \"garden/orchard\" {{ command {command:?} }}\n{}",
                if meadow {
                    "agent \"garden/meadow\" { command \"true\" }"
                } else {
                    ""
                }
            ),
            "amber",
        )
        .unwrap()
    }
    fn options(store: &Store, sequence: u64) -> owned_sets::Options {
        let current = store
            .owned_sets()
            .unwrap()
            .into_iter()
            .find(|v| v.id == "owned-set/garden");
        owned_sets::Options {
            set: "garden".into(),
            source: owned_sets::Source {
                repository: "acme/garden".into(),
                r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"),
                sequence,
            },
            expected_set: current.map_or("absent".into(), |v| v.revision),
            rollout: None,
            adopt: Default::default(),
            allow_empty: false,
            confirm_retire: None,
            expected_subjects: Default::default(),
        }
    }
    fn publish(store: &Store, input: &NormalizedIntent, sequence: u64) {
        let mut opts = options(store, sequence);
        opts.expected_subjects = store
            .owned_set_preview(input, &opts)
            .unwrap()
            .expected_subjects;
        store
            .apply_owned_set(input, &opts, &format!("set-{sequence}"), "person/operator")
            .unwrap();
    }
    fn share(from: &Store, to: &Store) {
        to.import_replication(&from.origin, &from.export_replication(0).unwrap())
            .unwrap();
    }
    type Fact = (ClaimRecord, canonical::ClaimKey, bool);
    /// Independent raw-history oracle/extraction is TEST ONLY, never operator apply/GET.
    fn facts(store: &Store) -> Vec<Fact> {
        let db = store.readers.get();
        let mut q=db.prepare("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims ORDER BY store_index").unwrap();
        q.query_map([],claim_from_row).unwrap().map(|c| {
            let c=c.unwrap();
            let rank=canonical::claim_key(&db,&c.id).unwrap();
            let eligible=!db.query_row("SELECT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=?1 AND state='repaired')",[&c.id],|r|r.get::<_,bool>(0)).unwrap();
            (c,rank,eligible)
        }).collect()
    }
    fn setup(store: &Store) {
        create_schema(&store.connection.write()).unwrap();
    }
    fn feed(store: &Store, ns: &str, fact: &Fact) -> Changes {
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let changes = apply(
            &tx,
            ns,
            None,
            Some(Captured {
                claim: &fact.0,
                rank: &fact.1,
                eligible: fact.2,
            }),
        )
        .unwrap();
        tx.commit().unwrap();
        changes
    }
    fn oracle(store: &Store, ns: &str) {
        let db = store.readers.get();
        let selected = owned_sets::selected(&db, None).unwrap();
        for view in &selected {
            let actual: String = db
                .query_row(
                    "SELECT body FROM local_agent_owned_selected WHERE namespace=?1 AND set_id=?2",
                    params![ns, view.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&actual).unwrap(),
                serde_json::to_value(view).unwrap()
            );
        }
        let members = selected
            .iter()
            .flat_map(|view| subjects(&db, ns, &view.id).unwrap())
            .collect::<BTreeSet<_>>();
        for subject in members {
            let actual = read_at(&db, ns, &subject).unwrap().unwrap();
            let Ok(owner) = owned_sets::owner(&db, &subject, None) else {
                assert!(!actual.blockers.is_empty());
                assert!(actual.selected.is_none());
                continue;
            };
            let owner = owner.unwrap();
            assert_eq!(actual.owners, vec![owner.clone()]);
            let view = selected.iter().find(|v| v.id == owner).unwrap();
            assert_eq!(actual.blockers, view.blockers);
            if !view.blockers.is_empty() {
                assert!(actual.selected.is_none());
                continue;
            }
            let effective = owned_sets::effective_members(&db, view, None).unwrap();
            let (member, retired, implicit) = effective.get(&subject).unwrap();
            let row = actual.selected.as_ref().unwrap();
            assert_eq!(
                (&row.member, row.retired, row.implicit),
                (member, *retired, *implicit)
            );
            assert_eq!(
                row.actor,
                store
                    .claim_by_id(&view.claim)
                    .unwrap()
                    .and_then(|c| c.actor)
            );
            assert_eq!(row.policy, view.receipt.rollout);
            let expected = owned_sets::desired_at(&db, &subject, store.index().unwrap()).unwrap();
            match (&row.desired, expected) {
                (None, None) => {}
                (Some(desired), Some(expected)) => {
                    assert_eq!(desired.kind, expected.kind);
                    assert_eq!(member.revision, expected.revision);
                    assert_eq!(member.claim, expected.claim_id);
                    assert_eq!(
                        canonical_json_text(&desired.desired).unwrap(),
                        expected.body
                    );
                    assert_eq!(
                        desired
                            .member
                            .as_ref()
                            .map(canonical_serialized_json_text)
                            .transpose()
                            .unwrap(),
                        expected.member
                    );
                    let manual = if desired.kind == "stop" {
                        owned_sets::manual_member(&db, member).unwrap()
                    } else {
                        crate::rollout::manual(desired)
                    };
                    assert_eq!(row.manual, manual);
                    if row.policy.is_some() || manual {
                        let rollout = actual.rollout().unwrap().unwrap();
                        assert_eq!(rollout.receipt, view.claim);
                        assert_eq!(rollout.desired_token, member.claim);
                        assert_eq!(rollout.target, crate::rollout::target(desired).unwrap());
                    }
                }
                _ => panic!("desired mismatch for {subject}"),
            }
        }
    }
    fn receipt_claim(store: &Store, set: &str, receipt: owned_sets::Revision) -> ClaimRecord {
        store
            .append_claim(&ClaimInput {
                subject: set.into(),
                kind: "owned-set.revised".into(),
                actor: Some("person/operator".into()),
                fields: serde_json::from_value(
                    json!({"revision":canonical_hash(&receipt).unwrap(),"body":receipt}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }

    #[test]
    fn populated_receipts_and_desired_converge_under_reverse_capture_and_duplicates() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", true), 10);
        publish(&store, &bundle("second", true), 20);
        setup(&store);
        let history = facts(&store);
        for fact in history.iter().rev() {
            feed(&store, "reverse", fact);
        }
        for fact in &history {
            feed(&store, "forward", fact);
        }
        oracle(&store, "reverse");
        oracle(&store, "forward");
        for fact in &history {
            assert!(feed(&store, "reverse", fact).changed.is_empty());
        }
        let db = store.readers.get();
        assert_eq!(
            read_at(&db, "reverse", "agent/garden/orchard").unwrap(),
            read_at(&db, "forward", "agent/garden/orchard").unwrap()
        );
    }

    #[test]
    fn late_losing_branch_members_are_implicitly_retired_without_latest_only_shortcut() {
        let amber = Store::open_memory("amber").unwrap();
        let cobalt = Store::open_memory("cobalt").unwrap();
        publish(&amber, &bundle("initial", false), 10);
        share(&amber, &cobalt);
        publish(&cobalt, &bundle("initial", true), 20);
        publish(&amber, &bundle("newer", false), 30);
        setup(&amber);
        for fact in facts(&amber) {
            feed(&amber, "n", &fact);
        }
        share(&cobalt, &amber);
        // Duplicates plus late lower-source arrivals do not promote receipt arrival order.
        for fact in facts(&amber).iter().rev() {
            feed(&amber, "n", fact);
        }
        oracle(&amber, "n");
        let row = read_at(&amber.readers.get(), "n", "agent/garden/meadow")
            .unwrap()
            .unwrap()
            .selected
            .unwrap();
        assert!(row.implicit && row.retired);
        assert_eq!(row.desired.unwrap().kind, "stop");
    }

    #[test]
    fn missing_sibling_declaration_wakes_unchanged_local_member_and_lineage_arrival() {
        let amber = Store::open_memory("amber").unwrap();
        let cobalt = Store::open_memory("cobalt").unwrap();
        publish(&amber, &bundle("old", false), 10);
        let input = bundle("future", true);
        let preview = cobalt
            .mission(
                &input,
                IntentInput {
                    kdl: "".into(),
                    source_name: None,
                },
            )
            .unwrap();
        cobalt
            .apply_as(
                &input,
                &preview.subject_tokens,
                "future",
                Some("person/operator"),
            )
            .unwrap();
        let future = cobalt
            .claims_for("agent/garden/meadow", Some("intent.desired"))
            .unwrap()
            .pop()
            .unwrap();
        let desired: DesiredSubject = serde_json::from_value(future.body.clone()).unwrap();
        let old = amber.owned_sets().unwrap().remove(0);
        let mut next = old.receipt.clone();
        next.previous = Some(format!("{}@{}", old.id, old.revision));
        next.source = options(&amber, 30).source;
        next.members.insert(
            future.subject.clone(),
            owned_sets::Member {
                manual_rollout: false,
                one_shot: false,
                kind: "agent".into(),
                claim: future.id.clone(),
                revision: desired_revision(&desired),
            },
        );
        receipt_claim(&amber, &old.id, next);
        setup(&amber);
        for fact in facts(&amber) {
            feed(&amber, "n", &fact);
        }
        oracle(&amber, "n");
        assert!(
            !read_at(&amber.readers.get(), "n", "agent/garden/orchard")
                .unwrap()
                .unwrap()
                .blockers
                .is_empty()
        );
        share(&cobalt, &amber);
        let fact = facts(&amber)
            .into_iter()
            .find(|f| f.0.id == future.id)
            .unwrap();
        let changes = feed(&amber, "n", &fact);
        assert!(changes.changed.contains("agent/garden/orchard"));
        assert!(changes.changed.contains("agent/garden/meadow"));
        oracle(&amber, "n");
    }

    #[test]
    fn cross_set_historical_ownership_conflict_retracts_and_unblocks_siblings() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", true), 10);
        setup(&store);
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        let receipt = store.owned_sets().unwrap().remove(0).receipt;
        let other = receipt_claim(&store, "owned-set/other", receipt);
        let fact = facts(&store)
            .into_iter()
            .find(|f| f.0.id == other.id)
            .unwrap();
        feed(&store, "n", &fact);
        oracle(&store, "n");
        let db = store.readers.get();
        assert!(
            read_at(&db, "n", "agent/garden/orchard")
                .unwrap()
                .unwrap()
                .selected
                .is_none()
        );
        drop(db);
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let change = apply(&tx, "n", Some(&fact.0), None).unwrap();
        assert!(change.changed.contains("agent/garden/meadow"));
        tx.commit().unwrap();
        assert!(
            read_at(&store.readers.get(), "n", "agent/garden/orchard")
                .unwrap()
                .unwrap()
                .blockers
                .is_empty()
        );
        // Retraction is captured operator behavior; raw source still has the conflicting claim.
    }

    #[test]
    fn authored_one_shot_retirement_and_rearm_match_real_store() {
        let store = Store::open_memory("amber").unwrap();
        let input=crate::graph::parse_owned_set_intent("version 2\nagent \"garden/orchard\" { one-shot; rollout \"manual\"; harness \"claude\" { model \"example-model\"; } }","amber").unwrap();
        publish(&store, &input, 10);
        setup(&store);
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        let subject = "agent/garden/orchard";
        let token = store.selected_desired_token(subject).unwrap().unwrap();
        let stop = parse_intent("version 2\nstop \"agent/garden/orchard\"", "amber").unwrap();
        store
            .apply_as(
                &stop,
                &BTreeMap::from([(subject.into(), vec![token])]),
                "retire",
                Some("daemon/runtime"),
            )
            .unwrap();
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        oracle(&store, "n");
        let row = read_at(&store.readers.get(), "n", subject)
            .unwrap()
            .unwrap()
            .selected
            .unwrap();
        assert!(row.retired && row.manual);
        assert!(!row.implicit);
        assert_eq!(row.desired.unwrap().kind, "stop");
        publish(&store,&crate::graph::parse_owned_set_intent("version 2\nagent \"garden/orchard\" { one-shot; rollout \"manual\"; harness \"claude\" { model \"next-model\"; } }","amber").unwrap(),20);
        for fact in facts(&store).iter().rev() {
            feed(&store, "n", fact);
        }
        oracle(&store, "n");
        assert_eq!(
            read_at(&store.readers.get(), "n", subject)
                .unwrap()
                .unwrap()
                .selected
                .unwrap()
                .desired
                .unwrap()
                .kind,
            "agent"
        );
    }

    #[test]
    fn accepted_receipt_repair_excludes_original_and_restores_previous_source() {
        let source = Store::open_memory("amber").unwrap();
        let target = Store::open_memory("cobalt").unwrap();
        publish(&source, &bundle("first", false), 10);
        let first = source.owned_sets().unwrap().remove(0);
        publish(&source, &bundle("second", false), 20);
        let second = source.owned_sets().unwrap().remove(0);
        super::super::tests::receive_and_project(
            &target,
            &source.origin,
            &super::super::tests::exchange_from(&source, &target.replication_inventory().unwrap()),
        );
        setup(&target);
        for fact in facts(&target) {
            feed(&target, "n", &fact);
        }
        let record = target
            .replica_records(false)
            .unwrap()
            .into_iter()
            .find(|r| r.claim_id.as_deref() == Some(second.claim.as_str()))
            .unwrap();
        target
            .connection
            .write()
            .execute(
                "UPDATE replica_records SET state='invalid' WHERE record_ref=?1",
                [&record.record_ref],
            )
            .unwrap();
        target
            .repair_replica_record(
                &record.record_ref,
                &first.claim,
                "receiver upgrade",
                "person/operator",
                "repair-receipt",
            )
            .unwrap();
        for fact in facts(&target) {
            feed(&target, "n", &fact);
        }
        oracle(&target, "n");
        assert_eq!(
            read_at(&target.readers.get(), "n", "agent/garden/orchard")
                .unwrap()
                .unwrap()
                .selected
                .unwrap()
                .receipt,
            first.claim
        );
    }

    #[test]
    fn same_source_and_lineage_conflicts_are_pending_and_unknown_kinds_do_not_change_keys() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", false), 10);
        let old = store.owned_sets().unwrap().remove(0);
        let mut receipt = old.receipt.clone();
        receipt.bundle_digest = "different".into();
        receipt.previous = Some("owned-set/garden@missing".into());
        receipt_claim(&store, &old.id, receipt);
        setup(&store);
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        oracle(&store, "n");
        assert!(
            !read_at(&store.readers.get(), "n", "agent/garden/orchard")
                .unwrap()
                .unwrap()
                .blockers
                .is_empty()
        );
        let unknown = store
            .append_claim(&ClaimInput {
                subject: "custom/future/unknown".into(),
                kind: "custom.future.unknown".into(),
                actor: None,
                fields: Default::default(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let fact = facts(&store)
            .into_iter()
            .find(|f| f.0.id == unknown.id)
            .unwrap();
        assert!(feed(&store, "n", &fact).changed.is_empty());
    }

    #[test]
    fn rollback_and_member_exhaustion_do_not_mutate_captured_namespace() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", false), 10);
        setup(&store);
        let history = facts(&store);
        for fact in &history {
            feed(&store, "n", fact);
        }
        let before = read_at(&store.readers.get(), "n", "agent/garden/orchard").unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        apply(&tx, "n", Some(&history.last().unwrap().0), None).unwrap();
        tx.rollback().unwrap();
        drop(writer);
        assert_eq!(
            read_at(&store.readers.get(), "n", "agent/garden/orchard").unwrap(),
            before
        );
        let mut receipt = store.owned_sets().unwrap().remove(0).receipt;
        let template = receipt.members.values().next().unwrap().clone();
        receipt.members = (0..MAX_MEMBERS + 1)
            .map(|i| (format!("agent/limit/{i}"), template.clone()))
            .collect();
        let raw = receipt_claim(&store, "owned-set/limit", receipt);
        let fact = facts(&store)
            .into_iter()
            .find(|f| f.0.id == raw.id)
            .unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(
            apply(
                &tx,
                "n",
                None,
                Some(Captured {
                    claim: &fact.0,
                    rank: &fact.1,
                    eligible: true
                })
            )
            .unwrap_err()
            .to_string()
            .contains("bound")
        );
        tx.commit().unwrap();
        drop(writer);
        assert!(store.claim_by_id(&raw.id).unwrap().is_some());
        assert_eq!(
            read_at(&store.readers.get(), "n", "agent/garden/orchard").unwrap(),
            before
        );
    }
    #[test]
    fn staged_pending_and_retired_namespace_reclamation_preserve_live_namespace() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", false), 10);
        setup(&store);
        let history = facts(&store);
        let declaration = history
            .iter()
            .find(|f| f.0.kind == "intent.desired" && f.0.body.get("owned_set").is_some())
            .unwrap();
        feed(&store, "retired", declaration);
        let pending = read_at(&store.readers.get(), "retired", "agent/garden/orchard")
            .unwrap()
            .unwrap();
        assert!(pending.owners.is_empty() && !pending.blockers.is_empty());
        for fact in &history {
            feed(&store, "retired", fact);
            feed(&store, "live", fact);
        }
        let before = read_at(&store.readers.get(), "live", "agent/garden/orchard").unwrap();
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        let mut pages = 0;
        while !reclaim_at(&tx, "retired", 1).unwrap() {
            pages += 1;
            assert!(pages < 100);
        }
        assert!(pages > 1);
        tx.commit().unwrap();
        drop(writer);
        assert!(
            read_at(&store.readers.get(), "retired", "agent/garden/orchard")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            read_at(&store.readers.get(), "live", "agent/garden/orchard").unwrap(),
            before
        );
    }
    #[test]
    fn irrelevant_heartbeats_never_consume_owned_retention_or_semantic_generations() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", false), 10);
        setup(&store);
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        let before = {
            let db = store.readers.get();
            (
            read_at(&db,"n","agent/garden/orchard").unwrap(),
            db.query_row("SELECT claims,bytes FROM local_agent_owned_state WHERE namespace='n'",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).unwrap(),
            db.query_row("SELECT generation FROM local_agent_owned_rows WHERE namespace='n' AND subject='agent/garden/orchard'",[],|r|r.get::<_,u64>(0)).unwrap()
        )
        };
        for i in 0..128 {
            let claim = store
                .append_claim(&ClaimInput {
                    subject: "agent/garden/orchard".into(),
                    kind: "harness.observed".into(),
                    actor: None,
                    fields: serde_json::from_value(
                        json!({"incarnation_id":"irrelevant-test","state":"idle","input_buffer":format!("heartbeat-{i}")}),
                    )
                    .unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            assert!(!relevant(&claim));
            let rank = canonical::claim_key(&store.readers.get(), &claim.id).unwrap();
            let changes = feed(&store, "n", &(claim, rank, true));
            assert!(changes.affected.is_empty() && changes.changed.is_empty());
        }
        let db = store.readers.get();
        assert_eq!(read_at(&db, "n", "agent/garden/orchard").unwrap(), before.0);
        assert_eq!(
            db.query_row(
                "SELECT claims,bytes FROM local_agent_owned_state WHERE namespace='n'",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            )
            .unwrap(),
            before.1
        );
        assert_eq!(db.query_row("SELECT generation FROM local_agent_owned_rows WHERE namespace='n' AND subject='agent/garden/orchard'",[],|r|r.get::<_,u64>(0)).unwrap(),before.2);
    }

    #[test]
    fn referenced_other_kind_fact_is_routed_from_captured_source_after_receipt_discovery() {
        let store = Store::open_memory("amber").unwrap();
        publish(&store, &bundle("first", false), 10);
        setup(&store);
        for fact in facts(&store) {
            feed(&store, "n", &fact);
        }
        let claim = store
            .append_claim(&ClaimInput {
                subject: "custom/future/member".into(),
                kind: "custom.future.member".into(),
                actor: None,
                fields: Default::default(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let rank = canonical::claim_key(&store.readers.get(), &claim.id).unwrap();
        assert!(
            feed(&store, "n", &(claim.clone(), rank.clone(), true))
                .affected
                .is_empty()
        );
        let view = store.owned_sets().unwrap().remove(0);
        let mut receipt = view.receipt.clone();
        receipt.previous = Some(format!("{}@{}", view.id, view.revision));
        receipt.source = options(&store, 20).source;
        receipt
            .members
            .get_mut("agent/garden/orchard")
            .unwrap()
            .claim = claim.id.clone();
        let published = receipt_claim(&store, &view.id, receipt);
        let fact = facts(&store)
            .into_iter()
            .find(|f| f.0.id == published.id)
            .unwrap();
        feed(&store, "n", &fact);
        assert!(referenced_at(&store.readers.get(), "n", &claim.id).unwrap());
        // The source adapter already captured this fact; replay its indexed identity, not GET history.
        let changed = feed(&store, "n", &(claim.clone(), rank.clone(), true));
        assert!(changed.changed.contains("agent/garden/orchard"));
        oracle(&store, "n");
        let db = store.readers.get();
        let row = read_at(&db, "n", "agent/garden/orchard").unwrap().unwrap();
        assert!(
            row.selected.is_none() && row.blockers.iter().any(|b| b.contains("invalid member"))
        );
        drop(db);
        // A relevant->irrelevant replacement still retracts the old indexed input. A known
        // reference keeps the other-kind fact as invalid evidence rather than silently ignoring it.
        let mut writer = store.connection.write();
        let tx = writer.transaction().unwrap();
        assert!(
            apply(&tx, "n", Some(&claim), None)
                .unwrap()
                .changed
                .contains("agent/garden/orchard")
        );
        tx.commit().unwrap();
    }
}

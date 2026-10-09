//! Registered custom projections. The signed claim log is authority; these tables are caches.
use super::*;
use serde::{Deserialize, Serialize};
use st3_schema::custom::{self as schema, Authority, Basis, Expr, Manifest, Predicate, Select};

// Explicit UPSERTs in triggers keep an outer admission UPSERT from overriding IGNORE.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS custom_registrations (
 subject TEXT PRIMARY KEY, kind TEXT NOT NULL, version INTEGER NOT NULL,
 prefix TEXT NOT NULL, hash TEXT NOT NULL, manifest TEXT NOT NULL,
 claim_id TEXT NOT NULL, state TEXT NOT NULL, error TEXT
);
CREATE INDEX IF NOT EXISTS custom_registrations_kind ON custom_registrations(kind,version);
CREATE TABLE IF NOT EXISTS custom_sources (
 subject TEXT PRIMARY KEY, registration TEXT NOT NULL, kind TEXT NOT NULL,
 revision TEXT NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL,
 person TEXT, episode TEXT, active INTEGER NOT NULL DEFAULT 0,
 requested_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS custom_sources_kind ON custom_sources(kind,subject);
CREATE INDEX IF NOT EXISTS custom_sources_kind_version ON custom_sources(kind,json_extract(body,'$.schema_version'),subject);
CREATE INDEX IF NOT EXISTS custom_sources_version ON custom_sources(json_extract(body,'$.schema_version'),subject);
CREATE INDEX IF NOT EXISTS custom_sources_attention ON custom_sources(person,subject) WHERE active=1;
CREATE TABLE IF NOT EXISTS custom_dependencies (
 source TEXT NOT NULL, dependency TEXT NOT NULL, kinds TEXT NOT NULL, revision TEXT NOT NULL,
 PRIMARY KEY(source,dependency,kinds)
);
CREATE INDEX IF NOT EXISTS custom_dependencies_input ON custom_dependencies(dependency,source);
CREATE TABLE IF NOT EXISTS local_custom_dirty (subject TEXT PRIMARY KEY);
CREATE TRIGGER IF NOT EXISTS custom_document_insert AFTER INSERT ON documents BEGIN
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency=NEW.name ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE kind='custom.st3-kinds.registered' AND json_extract(body,'$.fields.document')=NEW.name||'@'||NEW.hash ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_document_delete AFTER DELETE ON documents BEGIN
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency=OLD.name ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE kind='custom.st3-kinds.registered' AND json_extract(body,'$.fields.document')=OLD.name||'@'||OLD.hash ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_document_update AFTER UPDATE OF name,hash ON documents BEGIN
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency IN (OLD.name,NEW.name) ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE kind='custom.st3-kinds.registered' AND json_extract(body,'$.fields.document') IN (OLD.name||'@'||OLD.hash,NEW.name||'@'||NEW.hash) ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_claim_insert AFTER INSERT ON claims BEGIN
 INSERT INTO local_custom_dirty SELECT NEW.subject WHERE NEW.subject LIKE 'custom/%' AND NEW.subject NOT LIKE 'custom/client/%' ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE batch_id=NEW.batch_id AND store_index>NEW.store_index AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id) ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency IN (NEW.subject,NEW.id) ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE NEW.kind='doc.bound' AND kind='custom.st3-kinds.registered' AND substr(json_extract(body,'$.fields.document'),1,length(NEW.subject)+1)=NEW.subject||'@' ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_claim_delete AFTER DELETE ON claims BEGIN
 INSERT INTO local_custom_dirty SELECT OLD.subject WHERE OLD.subject LIKE 'custom/%' AND OLD.subject NOT LIKE 'custom/client/%' ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE batch_id=OLD.batch_id AND store_index>OLD.store_index AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id) ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency IN (OLD.subject,OLD.id) ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_claim_update AFTER UPDATE ON claims BEGIN
 INSERT INTO local_custom_dirty SELECT OLD.subject WHERE OLD.subject LIKE 'custom/%' AND OLD.subject NOT LIKE 'custom/client/%' ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT NEW.subject WHERE NEW.subject LIKE 'custom/%' AND NEW.subject NOT LIKE 'custom/client/%' ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency IN (OLD.subject,NEW.subject,OLD.id,NEW.id) ON CONFLICT(subject) DO NOTHING;
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE (OLD.batch_id IS NOT NEW.batch_id OR OLD.store_index IS NOT NEW.store_index) AND ((batch_id=OLD.batch_id AND store_index>OLD.store_index) OR (batch_id=NEW.batch_id AND store_index>NEW.store_index)) AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id) ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_batch_update AFTER UPDATE OF origin,replica_sequence ON batches BEGIN
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE batch_id=NEW.id ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_record_insert AFTER INSERT ON replica_records WHEN NEW.claim_id IS NOT NULL BEGIN
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE id=NEW.claim_id ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_record_update AFTER UPDATE OF position,claim_id,state ON replica_records BEGIN
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE id IN (OLD.claim_id,NEW.claim_id) ON CONFLICT(subject) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS custom_record_delete AFTER DELETE ON replica_records BEGIN
 INSERT INTO local_custom_dirty SELECT subject FROM claims WHERE id=OLD.claim_id ON CONFLICT(subject) DO NOTHING;
END;
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationRequest {
    pub manifest: Manifest,
    pub actor: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyRequest {
    pub subject: String,
    pub registration: String,
    pub revision: String,
    pub episode: String,
    pub fields: BTreeMap<String, Value>,
    pub actor: String,
    pub idempotency_key: String,
}

fn typed(e: st3_schema::ValidationError) -> St3Error {
    St3Error::new(e.code, e.message)
}
fn fail(message: impl Into<String>) -> St3Error {
    St3Error::new("invalid-custom-claim", message)
}
fn fields(claim: &ClaimRecord) -> BTreeMap<String, Value> {
    claim
        .body
        .get("fields")
        .and_then(Value::as_object)
        .map(|v| v.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}
fn claims(connection: &Connection, subject: &str) -> Result<Vec<ClaimRecord>> {
    connection.prepare(&canonical_sql(
        "SELECT claims.id,claims.store_index,claims.batch_id,claims.subject,claims.kind,claims.origin,claims.actor,claims.body,claims.predecessors,claims.accepted_at_unix_ms
         FROM claims JOIN batches ON batches.id=claims.batch_id WHERE claims.subject=?1
         AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')
         ORDER BY CANONICAL_ASC(claims)"))?
        .query_map([subject],claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
fn registration(
    connection: &Connection,
    subject: &str,
) -> Result<Option<(String, Manifest, String)>> {
    let row: Option<(String,String,String)> = connection.query_row(
        "SELECT hash,manifest,state FROM custom_registrations WHERE substr(?1,1,length(prefix))=prefix ORDER BY subject LIMIT 1",[subject],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    row.map(|(hash, m, state)| Ok((hash, serde_json::from_str(&m)?, state)))
        .transpose()
}
pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}
pub(super) fn open(tx: &Transaction<'_>) -> Result<()> {
    if tx
        .query_row(
            "SELECT value FROM meta WHERE key='custom_projections_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        != Some("1")
    {
        rebuild(tx)
    } else {
        flush(tx)
    }
}
pub(super) fn rebuild(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("DELETE FROM custom_registrations; DELETE FROM custom_sources; DELETE FROM custom_dependencies; DELETE FROM local_custom_dirty; INSERT INTO local_custom_dirty SELECT DISTINCT subject FROM claims WHERE subject LIKE 'custom/%' AND subject NOT LIKE 'custom/client/%';")?;
    flush(tx)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('custom_projections_version','1')",
        [],
    )?;
    Ok(())
}
pub(super) fn flush(tx: &Transaction<'_>) -> Result<()> {
    if !tx.query_row("SELECT EXISTS(SELECT 1 FROM local_custom_dirty)", [], |r| {
        r.get::<_, bool>(0)
    })? {
        return Ok(());
    }
    tx.execute("INSERT OR IGNORE INTO local_custom_dirty SELECT source FROM custom_dependencies WHERE dependency IN (SELECT subject FROM local_custom_dirty)", [])?;
    let registry_dirty: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_custom_dirty WHERE subject LIKE 'custom/st3-kinds/%')",
        [],
        |r| r.get(0),
    )?;
    if registry_dirty {
        let before = registration_rows(tx)?;
        refresh_registrations(tx)?;
        let after = registration_rows(tx)?;
        let changed = before
            .iter()
            .chain(&after)
            .filter(|(id, _)| before.get(*id) != after.get(*id))
            .map(|(_, (prefix, _))| prefix.clone())
            .collect::<BTreeSet<_>>();
        for prefix in changed {
            tx.execute("INSERT OR IGNORE INTO local_custom_dirty SELECT DISTINCT subject FROM claims WHERE subject>=?1 AND subject<?2",params![prefix,prefix_end(&prefix)])?;
        }
    }
    let dirty=tx.prepare("SELECT subject FROM local_custom_dirty WHERE subject LIKE 'custom/%' AND subject NOT LIKE 'custom/client/%' AND subject NOT LIKE 'custom/st3-kinds/%' ORDER BY subject")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    // Build dependency edges before checking cycles, so arrival order cannot choose health.
    for _ in 0..2 {
        for subject in &dirty {
            tx.execute_batch("SAVEPOINT custom_source")?;
            let result = refresh_source(tx, subject);
            if result.is_err() {
                tx.execute_batch("ROLLBACK TO custom_source; RELEASE custom_source")?;
                result?;
            } else {
                tx.execute_batch("RELEASE custom_source")?;
            }
        }
    }
    tx.execute("DELETE FROM local_custom_dirty", [])?;
    Ok(())
}
fn prefix_end(prefix: &str) -> String {
    // Validated prefixes end in '/', whose ASCII successor bounds the subject index.
    format!("{}0", prefix.strip_suffix('/').unwrap_or(prefix))
}
fn registration_rows(connection: &Connection) -> Result<BTreeMap<String, (String, Value)>> {
    connection
        .prepare(
            "SELECT subject,prefix,hash,manifest,claim_id,state,error FROM custom_registrations",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, String>(1)?,
                    json!([
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?
                    ]),
                ),
            ))
        })?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}
fn refresh_registrations(tx: &Transaction<'_>) -> Result<()> {
    tx.execute("DELETE FROM custom_registrations", [])?;
    let subjects=tx.prepare("SELECT DISTINCT subject FROM claims WHERE kind='custom.st3-kinds.registered' ORDER BY subject")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for subject in subjects {
        let records = claims(tx, &subject)?;
        let mut registrations = BTreeMap::new();
        for c in records.iter().filter(|c| c.kind == schema::REGISTERED) {
            if !c
                .actor
                .as_deref()
                .is_some_and(|a| a.starts_with("agent/") || a.starts_with("person/"))
            {
                continue;
            }
            let f = fields(c);
            let Some(reference) = f.get("document").and_then(Value::as_str) else {
                continue;
            };
            let Ok((name, hash)) = split_document_ref(reference) else {
                continue;
            };
            let bytes:Option<Vec<u8>>=tx.query_row("SELECT b.bytes FROM documents d JOIN blobs b ON b.hash=d.hash WHERE d.name=?1 AND d.hash=?2",params![name,hash],|r|r.get(0)).optional()?;
            let Some(bytes) = bytes else { continue };
            if bytes.len() > schema::MAX_BYTES || hex::encode(Sha256::digest(&bytes)) != hash {
                continue;
            }
            let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else {
                continue;
            };
            if m.validate().is_err() || m.subject() != subject {
                continue;
            }
            registrations.entry(hash.to_owned()).or_insert((m, c));
        }
        if let Some((hash, (m, c))) = registrations.first_key_value() {
            let state = if registrations.len() == 1 {
                "ready"
            } else {
                "conflict"
            };
            tx.execute(
                "INSERT INTO custom_registrations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    subject,
                    m.kind,
                    m.version,
                    m.subject_prefix,
                    hash,
                    canonical_serialized_json_text(m)?,
                    c.id,
                    state,
                    if state == "conflict" {
                        Some("immutable registration has competing hashes")
                    } else {
                        None
                    }
                ],
            )?;
        }
    }
    // Overlapping replicated registrations cannot silently capture each other's subjects.
    tx.execute("UPDATE custom_registrations SET state='conflict',error='overlapping registration prefixes' WHERE EXISTS(SELECT 1 FROM custom_registrations other WHERE other.subject<>custom_registrations.subject AND (substr(other.prefix,1,length(custom_registrations.prefix))=custom_registrations.prefix OR substr(custom_registrations.prefix,1,length(other.prefix))=other.prefix))",[])?;
    Ok(())
}

/// Local writes use this before appending. Replicated facts are validated again in the fold,
/// including facts received before the immutable manifest or their creation claim.
pub(super) fn prepare(
    tx: &Transaction<'_>,
    input: &ClaimInput,
) -> Result<Option<BTreeMap<String, Value>>, St3Error> {
    if input.subject.starts_with(schema::REGISTRY_PREFIX) || input.kind == schema::REGISTERED {
        return Err(St3Error::new(
            "custom-registration-only",
            "use schema register to publish a registration",
        ));
    }
    if !input.subject.starts_with("custom/") {
        return Ok(None);
    }
    flush(tx).map_err(internal)?;
    let Some((hash, m, state)) = registration(tx, &input.subject).map_err(internal)? else {
        if input.fields.contains_key("_registration") {
            return Err(fail("registration is not available"));
        }
        return Ok(None);
    };
    if state != "ready" {
        return Err(fail("registration is conflicting"));
    }
    let mut f = input.fields.clone();
    if f.get("_registration")
        .is_some_and(|v| v.as_str() != Some(&hash))
    {
        return Err(fail("claim pins another registration"));
    }
    f.insert("_registration".into(), json!(hash));
    let history = claims(tx, &input.subject).map_err(internal)?;
    let creation = history.iter().find(|c| {
        c.kind == m.creation_kind && fields(c).get("_registration") == Some(&json!(hash))
    });
    validate_fact(tx, &m, &input.kind, input.actor.as_deref(), &f, creation)
        .map_err(|e| fail(e.to_string()))?;
    Ok(Some(f))
}
fn validate_fact(
    connection: &Connection,
    m: &Manifest,
    kind: &str,
    actor: Option<&str>,
    f: &BTreeMap<String, Value>,
    creation: Option<&ClaimRecord>,
) -> Result<()> {
    let spec = m
        .claims
        .get(kind)
        .ok_or_else(|| anyhow::anyhow!("claim kind is not registered for this subject"))?;
    schema::validate_fields(spec, f)?;
    let actor = actor
        .filter(|a| a.starts_with("agent/") || a.starts_with("person/"))
        .ok_or_else(|| anyhow::anyhow!("typed claim needs a concrete actor"))?;
    st3_schema::registry().validate_subject(actor)?;
    anyhow::ensure!(
        actor.starts_with("agent/") || actor.starts_with("person/"),
        "typed actor must be a person or agent"
    );
    match spec.authority {
        Authority::Creator => {
            if let Some(c) = creation {
                anyhow::ensure!(
                    c.actor.as_deref() == Some(actor),
                    "only the creator may append creation facts"
                );
            }
        }
        Authority::Owner => {
            anyhow::ensure!(
                creation.and_then(|c| c.actor.as_deref()) == Some(actor),
                "only the request owner may write this claim"
            );
        }
        Authority::Recipient => {
            let attention = m
                .attention
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("recipient is undeclared"))?;
            let recipient = creation
                .and_then(|c| {
                    c.body
                        .get("fields")
                        .and_then(|f| f.get(&attention.recipient_field))
                })
                .and_then(Value::as_str);
            anyhow::ensure!(
                recipient == Some(actor),
                "only the addressed person may write this claim"
            );
        }
    }
    for (name, spec) in &spec.fields {
        if let Some(v) = f.get(name).and_then(Value::as_str) {
            if spec.document {
                validate_documents_connection(connection, v)?;
            }
            if spec.claim {
                anyhow::ensure!(
                    claim_by_id_tx(connection, v)?.is_some(),
                    "claim reference is not available"
                );
            }
        }
    }
    if let Some(v) = f.get("_basis") {
        let basis: Vec<Basis> = serde_json::from_value(v.clone())?;
        anyhow::ensure!(
            !basis.is_empty() && basis.len() <= 256,
            "basis needs 1..256 dependencies"
        );
        for b in basis {
            st3_schema::registry().validate_subject(&b.subject)?;
            anyhow::ensure!(
                !b.kinds.is_empty()
                    && b.kinds.len() <= 32
                    && b.kinds
                        .iter()
                        .all(|k| st3_schema::is_custom_claim_kind(k) && k != kind)
                    && b.revision.len() == 64,
                "invalid basis filter or revision"
            );
        }
    }
    Ok(())
}
fn validate_documents_connection(connection: &Connection, reference: &str) -> Result<()> {
    let (name, hash) = split_document_ref(reference)?;
    let found:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM documents d JOIN blobs b ON b.hash=d.hash WHERE d.name=?1 AND d.hash=?2)",params![name,hash],|r|r.get(0))?;
    anyhow::ensure!(found, "document reference is not available");
    Ok(())
}
fn eval(expr: &Expr, slots: &BTreeMap<String, ClaimRecord>) -> Value {
    match expr {
        Expr::Field { slot, field } => slots
            .get(slot)
            .and_then(|c| c.body.get("fields"))
            .and_then(|v| v.get(field))
            .cloned()
            .unwrap_or(Value::Null),
        Expr::Actor { slot } => slots
            .get(slot)
            .and_then(|c| c.actor.clone())
            .map(Value::String)
            .unwrap_or(Value::Null),
        Expr::ClaimId { slot } => slots.get(slot).map(|c| json!(c.id)).unwrap_or(Value::Null),
        Expr::Constant { value } => value.clone(),
    }
}
fn predicate(p: &Predicate, slots: &BTreeMap<String, ClaimRecord>) -> bool {
    match p {
        Predicate::Exists { slot } => slots.contains_key(slot),
        Predicate::Eq { left, right } => {
            let a = eval(left, slots);
            let b = eval(right, slots);
            !a.is_null() && !b.is_null() && a == b
        }
        Predicate::All { args } => args.iter().all(|p| predicate(p, slots)),
        Predicate::Any { args } => args.iter().any(|p| predicate(p, slots)),
        Predicate::Not { arg } => !predicate(arg, slots),
    }
}
fn slots(m: &Manifest, history: &[ClaimRecord]) -> BTreeMap<String, ClaimRecord> {
    m.slots
        .iter()
        .filter_map(|(name, s)| {
            let iter = history.iter().filter(|c| c.kind == s.kind);
            let c = match s.select {
                Select::First => iter.into_iter().next(),
                Select::Last => iter.into_iter().next_back(),
            };
            c.map(|c| (name.clone(), c.clone()))
        })
        .collect()
}
fn basis_revision(connection: &Connection, subject: &str, kinds: &[String]) -> Result<String> {
    let history = claims(connection, subject)?;
    let mut kinds = kinds.to_vec();
    kinds.sort();
    kinds.dedup();
    canonical_hash(&(
        subject,
        &kinds,
        history
            .iter()
            .filter(|c| kinds.contains(&c.kind))
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
    ))
}
fn cycle(connection: &Connection, source: &str, dependency: &str) -> Result<bool> {
    let mut visited = BTreeSet::new();
    let mut pending = vec![dependency.to_owned()];
    while let Some(subject) = pending.pop() {
        if subject == source {
            return Ok(true);
        }
        if !visited.insert(subject.clone()) {
            continue;
        }
        anyhow::ensure!(visited.len() <= 256, "dependency traversal exceeds bound");
        pending.extend(
            connection
                .prepare("SELECT dependency FROM custom_dependencies WHERE source=?1")?
                .query_map([&subject], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(false)
}
fn refresh_source(tx: &Transaction<'_>, subject: &str) -> Result<()> {
    let Some((hash, m, registration_state)) = registration(tx, subject)? else {
        let history = claims(tx, subject)?;
        if let Some(c) = history
            .iter()
            .find(|c| fields(c).contains_key("_registration"))
        {
            let hash = fields(c)["_registration"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let revision =
                canonical_hash(&history.iter().map(|c| c.id.as_str()).collect::<Vec<_>>())?;
            let body = json!({"id":subject,"kind":"custom-subject","registered_kind":"","registration":hash,"revision":revision,"state":"pending-dependencies","fields":{},"updated_at":timestamp(c.accepted_at_unix_ms)});
            tx.execute("INSERT OR REPLACE INTO custom_sources VALUES(?1,?2,'',?3,'pending-dependencies',?4,NULL,NULL,0,?5)",params![subject,hash,revision,canonical_json_text(&body)?,c.accepted_at_unix_ms.to_string()])?;
            return Ok(());
        }
        tx.execute("DELETE FROM custom_sources WHERE subject=?1", [subject])?;
        tx.execute("DELETE FROM custom_dependencies WHERE source=?1", [subject])?;
        return Ok(());
    };
    let history = claims(tx, subject)?;
    let pinned = history
        .iter()
        .filter(|c| fields(c).get("_registration") == Some(&json!(hash)))
        .cloned()
        .collect::<Vec<_>>();
    let creation = pinned.iter().find(|c| c.kind == m.creation_kind);
    let mut error = None;
    let mut valid = Vec::new();
    for c in &pinned {
        match validate_fact(tx, &m, &c.kind, c.actor.as_deref(), &fields(c), creation) {
            Ok(()) => valid.push(c.clone()),
            Err(e) => {
                error = Some(e.to_string());
            }
        }
    }
    let slots = slots(&m, &valid);
    let revision = canonical_hash(&(
        hash.as_str(),
        pinned.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
    ))?;
    let mut state = if registration_state != "ready" {
        "conflict"
    } else if error
        .as_ref()
        .is_some_and(|e| !e.contains("reference is not available"))
        && creation.is_some()
    {
        "invalid"
    } else if creation.is_none() || error.is_some() {
        "pending-dependencies"
    } else {
        "ready"
    };
    let mut basis = Vec::new();
    for c in slots.values() {
        if let Some(v) = fields(c).get("_basis")
            && let Ok(b) = serde_json::from_value::<Vec<Basis>>(v.clone())
        {
            basis.extend(b);
        }
    }
    tx.execute("DELETE FROM custom_dependencies WHERE source=?1", [subject])?;
    if basis.len() > 256 {
        state = "invalid";
        error = Some("selected slots exceed 256 basis dependencies".into());
        basis.clear();
    }
    for b in &basis {
        tx.execute(
            "INSERT OR REPLACE INTO custom_dependencies VALUES(?1,?2,?3,?4)",
            params![
                subject,
                b.subject,
                canonical_serialized_json_text(&b.kinds)?,
                b.revision
            ],
        )?;
    }
    for b in &basis {
        if b.subject == subject
            && pinned
                .iter()
                .any(|c| b.kinds.contains(&c.kind) && fields(c).contains_key("_basis"))
        {
            state = "invalid";
            error = Some("same-source basis must select raw input kinds".into());
        }
        match if b.subject == subject {
            Ok(false)
        } else {
            cycle(tx, subject, &b.subject)
        } {
            Ok(false) => {}
            Ok(true) => {
                state = "invalid";
                error = Some("cyclic derived state basis".into());
            }
            Err(e) => {
                state = "invalid";
                error = Some(e.to_string());
            }
        }
        if state == "ready" && basis_revision(tx, &b.subject, &b.kinds)? != b.revision {
            state = "stale";
        }
    }
    // Document arrival retries only sources that actually reference that immutable document.
    for c in &pinned {
        let Some(specification) = m.claims.get(&c.kind) else {
            continue;
        };
        for (name, spec) in &specification.fields {
            if spec.claim
                && let Some(reference) = fields(c).get(name).and_then(Value::as_str)
            {
                tx.execute(
                    "INSERT OR IGNORE INTO custom_dependencies VALUES(?1,?2,'[]','')",
                    params![subject, reference],
                )?;
            }
            if spec.document
                && let Some(reference) = fields(c).get(name).and_then(Value::as_str)
                && let Ok((name, _)) = split_document_ref(reference)
            {
                tx.execute(
                    "INSERT OR IGNORE INTO custom_dependencies VALUES(?1,?2,'[]','')",
                    params![subject, name],
                )?;
            }
        }
    }
    let mut replies: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for c in &pinned {
        if let (Some(revision), Some(digest)) = (
            c.body
                .pointer("/_custom_reply/revision")
                .and_then(Value::as_str),
            c.body
                .pointer("/_custom_reply/digest")
                .and_then(Value::as_str),
        ) {
            replies
                .entry(revision.into())
                .or_default()
                .entry(digest.into())
                .or_default()
                .push(c.id.clone());
        }
    }
    let reply_conflicts = replies
        .into_iter()
        .filter(|(_, variants)| variants.len() > 1)
        .map(|(revision, variants)| {
            let mut ids = variants.into_values().flatten().collect::<Vec<_>>();
            ids.sort();
            json!({"revision":revision,"claims":ids})
        })
        .collect::<Vec<_>>();
    if state == "ready" && !reply_conflicts.is_empty() {
        state = "conflict";
    }
    let output = m
        .fields
        .iter()
        .map(|(n, e)| (n.clone(), eval(e, &slots)))
        .collect::<BTreeMap<_, _>>();
    let mut body = json!({"kind":"custom-subject","id":subject,"registered_kind":m.kind,"schema_version":m.version,"registration":hash,"revision":revision,"state":state,"owner":creation.and_then(|c|c.actor.as_deref()),"fields":output,"provenance":slots.iter().map(|(n,c)|(n.clone(),json!({"claim_id":c.id,"actor":c.actor}))).collect::<BTreeMap<_,_>>(),"input_count":pinned.len()});
    if !reply_conflicts.is_empty() {
        body["reply_conflicts"] = json!(reply_conflicts);
    }
    if let Some(e) = error {
        body["error"] = json!(e);
    }
    let mut person = None;
    let mut episode = None;
    let mut active = false;
    if let Some(a) = &m.attention {
        person = creation
            .and_then(|c| c.body.get("fields"))
            .and_then(|f| f.get(&a.recipient_field))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let title = eval(&a.title, &slots);
        let detail = eval(&a.detail, &slots);
        let ep = eval(&a.episode, &slots);
        if state == "ready" && predicate(&a.when, &slots) {
            if !title
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 512)
                || !detail.as_str().is_some_and(|s| s.len() <= 8192)
                || !ep.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256)
                || person.is_none()
            {
                state = "invalid";
                body["state"] = json!(state);
                body["error"] = json!("attention output type/size is invalid");
            } else {
                active = true;
                episode = ep.as_str().map(str::to_owned);
            }
        }
        body["attention"] = json!({"title":title,"detail":detail,"episode":ep,"recipient":person,"active":active,"reply":a.reply});
    }
    if serde_json::to_vec(&body)?.len() > schema::MAX_BYTES {
        active = false;
        state = "invalid";
        body = json!({"kind":"custom-subject","id":subject,"registered_kind":m.kind,"schema_version":m.version,"registration":hash,"revision":revision,"state":state,"error":"projected row exceeds 64 KiB"});
    }
    let requested_at = creation.map_or(0, |c| c.accepted_at_unix_ms).to_string();
    body["updated_at"] = json!(timestamp(requested_at.parse().unwrap_or(0)));
    tx.execute("INSERT INTO custom_sources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(subject) DO UPDATE SET registration=excluded.registration,kind=excluded.kind,revision=excluded.revision,state=excluded.state,body=excluded.body,person=excluded.person,episode=excluded.episode,active=excluded.active,requested_at=excluded.requested_at",params![subject,hash,m.kind,revision,state,canonical_json_text(&body)?,person,episode,active,requested_at])?;
    Ok(())
}

impl Store {
    pub fn register_custom_kind(&self, request: &RegistrationRequest) -> Result<Value, St3Error> {
        request.manifest.validate().map_err(typed)?;
        validate_actor(&request.actor)?;
        if !request.actor.starts_with("agent/") && !request.actor.starts_with("person/") {
            return Err(fail("registration needs a concrete actor"));
        }
        let m = &request.manifest;
        let bytes = serde_json::to_vec(m).map_err(internal)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        let name = format!("doc/custom-kinds/{}.v{}/{}", m.kind, m.version, hash);
        self.put_document_as(
            &name,
            &bytes,
            &None,
            &format!("custom-manifest:{hash}"),
            Some(&request.actor),
        )?;
        self.connection.batched(|tx|->Result<Value,St3Error>{
            flush(tx).map_err(internal)?;
            let existing:Option<(String,String)>=tx.query_row("SELECT hash,state FROM custom_registrations WHERE subject=?1",[m.subject()],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(internal)?;
            if let Some((prior,state))=existing {
                if prior==hash&&state=="ready" {return Ok(json!({"id":m.subject(),"registration":hash,"state":state}));}
                return Err(fail("kind/version is immutable; publish a new version"));
            }
            let overlapping:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM custom_registrations WHERE substr(prefix,1,length(?1))=?1 OR substr(?1,1,length(prefix))=prefix)",[&m.subject_prefix],|r|r.get(0)).map_err(internal)?;
            let upper = prefix_end(&m.subject_prefix);
            let populated:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM claims WHERE subject>=?1 AND subject<?2)",params![m.subject_prefix,upper],|r|r.get(0)).map_err(internal)?;
            if overlapping||populated {return Err(fail("subject prefix overlaps a registration or existing untyped facts"));}
            smallclaims::store::principals::rules_gate_tx(tx,&self.origin,&request.actor,schema::REGISTERED,&m.subject()).map_err(internal)?;
            append_claim_tx(tx,&self.origin,&m.subject(),schema::REGISTERED,Some(&request.actor),&json!({"fields":{"document":format!("{name}@{hash}")}}),&[],None).map_err(internal)?;
            flush(tx).map_err(internal)?;
            Ok(json!({"id":m.subject(),"registration":hash,"state":"ready"}))
        }).map_err(internal)?
    }
    pub fn custom_registrations(&self) -> Result<Vec<Value>> {
        self.readers.get().prepare("SELECT subject,hash,manifest,state,error FROM custom_registrations ORDER BY subject")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?)))?.map(|r|{let(s,h,m,state,error)=r?;Ok(json!({"id":s,"registration":h,"manifest":serde_json::from_str::<Value>(&m)?,"state":state,"error":error}))}).collect()
    }
    pub fn custom_subject(&self, subject: &str) -> Result<Option<Value>> {
        source(&self.readers.get(), subject)
    }
    /// Which of these subjects are registered custom sources, in one read.
    pub fn registered_custom_subjects(&self, subjects: &[String]) -> Result<BTreeSet<String>> {
        if subjects.is_empty() {
            return Ok(BTreeSet::new());
        }
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(
            "SELECT subject, body FROM custom_sources
             WHERE subject IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = statement.query_map([serde_json::to_string(subjects)?], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut registered = BTreeSet::new();
        for row in rows {
            let (subject, body) = row?;
            // `custom_subject` reports a source whose body does not parse as unregistered.
            if serde_json::from_str::<Value>(&body).is_ok() {
                registered.insert(subject);
            }
        }
        Ok(registered)
    }
    pub fn custom_subjects(
        &self,
        kind: Option<&str>,
        version: Option<u32>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let connection = self.readers.get();
        let query = match (kind, version) {
            (Some(_), Some(_)) => {
                "SELECT body FROM custom_sources WHERE kind=?1 AND json_extract(body,'$.schema_version')=?2 AND subject>?3 ORDER BY subject LIMIT ?4"
            }
            (Some(_), None) => {
                "SELECT body FROM custom_sources WHERE kind=?1 AND subject>?3 ORDER BY subject LIMIT ?4"
            }
            (None, Some(_)) => {
                "SELECT body FROM custom_sources WHERE json_extract(body,'$.schema_version')=?2 AND subject>?3 ORDER BY subject LIMIT ?4"
            }
            (None, None) => {
                "SELECT body FROM custom_sources WHERE subject>?3 ORDER BY subject LIMIT ?4"
            }
        };
        let texts = connection
            .prepare(query)?
            .query_map(
                params![kind, version, after.unwrap_or(""), limit.min(501)],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        texts
            .into_iter()
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .collect()
    }
    pub fn custom_basis_revision(&self, subject: &str, kinds: &[String]) -> Result<String> {
        basis_revision(&self.readers.get(), subject, kinds)
    }
    pub(super) fn custom_attention_items(
        &self,
        person: Option<&str>,
    ) -> Result<Vec<AttentionItemView>> {
        let connection = self.readers.get();
        let rows = if let Some(person) = person {
            connection.prepare("SELECT body,requested_at FROM custom_sources WHERE active=1 AND person=?1 ORDER BY subject")?
                .query_map([person], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            connection
                .prepare(
                    "SELECT body,requested_at FROM custom_sources WHERE active=1 ORDER BY subject",
                )?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        rows.into_iter()
            .map(|(text, at)| {
                attention_item(&serde_json::from_str(&text)?, at.parse().unwrap_or(0))
            })
            .collect()
    }
    pub fn reply_custom_subject(&self, request: &ReplyRequest) -> Result<ClaimRecord, St3Error> {
        validate_actor(&request.actor)?;
        if request.idempotency_key.is_empty() || request.idempotency_key.len() > 256 {
            return Err(fail("reply needs a bounded idempotency key"));
        }
        let digest = canonical_hash(request).map_err(internal)?;
        let key = canonical_hash(&("custom.reply", &request.actor, &request.idempotency_key))
            .map_err(internal)?;
        self.connection.batched(|tx|->Result<ClaimRecord,St3Error>{
            let prior=claims(tx,&request.subject).map_err(internal)?.into_iter().filter(|c|c.body.pointer("/_custom_reply/key")==Some(&json!(key))).collect::<Vec<_>>();
            if let Some(c)=prior.first() {
                if prior.iter().any(|c|c.body.pointer("/_custom_reply/digest")!=Some(&json!(digest))) { return Err(St3Error::new("idempotency-conflict","reply key identifies competing requests")); }
                return Ok(c.clone());
            }
            flush(tx).map_err(internal)?;
            let view=source(tx,&request.subject).map_err(internal)?.ok_or_else(||fail("custom source is unavailable"))?;
            if view["registration"]!=request.registration||view["revision"]!=request.revision||view["state"]!="ready"||view["attention"]["active"]!=true||view["attention"]["episode"]!=request.episode {return Err(St3Error::new("stale-fence","custom source revision/episode changed"));}
            if view["attention"]["recipient"]!=request.actor {return Err(St3Error::new("forbidden","only the addressed person may reply"));}
            let (_,m,_)=registration(tx,&request.subject).map_err(internal)?.ok_or_else(||fail("registration disappeared"))?;
            let reply=&m.attention.as_ref().ok_or_else(||fail("no reply action"))?.reply;
            if request.fields.keys().any(|name|!reply.fields.contains_key(name)) { return Err(fail("reply contains undeclared or reserved input fields")); }
            schema::validate_fields(&schema::ClaimSchema{authority:Authority::Recipient,fields:reply.fields.clone(),additional_fields:false},&request.fields).map_err(typed)?;
            let history=claims(tx,&request.subject).map_err(internal)?.into_iter().filter(|c|fields(c).get("_registration")==Some(&json!(request.registration))).collect::<Vec<_>>();
            let slots=slots(&m,&history);
            let mut f=request.fields.clone();
            for(n,e)in &reply.bindings {f.insert(n.clone(),eval(e,&slots));}
            let input=ClaimInput{subject:request.subject.clone(),kind:reply.kind.clone(),actor:Some(request.actor.clone()),fields:f,evidence:vec![],expected_subject:None,idempotency_key:None};
            let f=prepare(tx,&input)?.ok_or_else(||fail("registration disappeared"))?;
            smallclaims::store::principals::rules_gate_tx(tx,&self.origin,&request.actor,&reply.kind,&request.subject).map_err(internal)?;
            let predecessor=latest_claim_id_tx(tx,&request.subject).map_err(internal)?.into_iter().collect::<Vec<_>>();
            let c=append_claim_tx(tx,&self.origin,&request.subject,&reply.kind,Some(&request.actor),&json!({"fields":f,"_custom_reply":{"revision":request.revision,"episode":request.episode,"key":key,"digest":digest}}),&predecessor,None).map_err(internal)?;
            flush(tx).map_err(internal)?;
            Ok(c)
        }).map_err(internal)?
    }
}
/// One canonical custom source, shared by the collection reader and keyed maintenance.
pub(super) fn attention_item(body: &Value, at: u128) -> Result<AttentionItemView> {
    let a = &body["attention"];
    Ok(AttentionItemView {
        episode: a["episode"].as_str().unwrap_or_default().into(),
        priority: "normal".into(),
        kind: format!(
            "custom.{}",
            body["registered_kind"].as_str().unwrap_or_default()
        ),
        review_mode: None,
        subject: body["id"].as_str().unwrap_or_default().into(),
        person: a["recipient"].as_str().unwrap_or_default().into(),
        requester_id: body["owner"].as_str().map(str::to_owned),
        launch_id: None,
        variant_id: None,
        message_id: None,
        title: a["title"].as_str().unwrap_or_default().into(),
        detail: a["detail"].as_str().unwrap_or_default().into(),
        request: None,
        mission: None,
        mission_run: None,
        step: None,
        targets: vec![],
        requested_at_unix_ms: at,
        actions: vec![attention_action(
            "reply",
            &[
                "st",
                "subject",
                "reply",
                body["id"].as_str().unwrap_or_default(),
                "--fields-file",
                "REPLY.json",
                "--registration",
                body["registration"].as_str().unwrap_or_default(),
                "--revision",
                body["revision"].as_str().unwrap_or_default(),
                "--episode",
                a["episode"].as_str().unwrap_or_default(),
                "--idempotency-key",
                "REPLY-KEY",
                "--as",
                a["recipient"].as_str().unwrap_or_default(),
            ],
        )],
    })
}

fn source(connection: &Connection, subject: &str) -> Result<Option<Value>> {
    let text: Option<String> = connection
        .query_row(
            "SELECT body FROM custom_sources WHERE subject=?1",
            [subject],
            |r| r.get(0),
        )
        .optional()?;
    text.map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}

fn timestamp(at: u128) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(at).unwrap_or(0))
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests;

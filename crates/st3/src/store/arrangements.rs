//! Admission-time register heads. Durable edits are replay inputs, never a read/validation path.
use super::*;
use st3_schema::arrangements::{self as schema, Operation};

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS arrangements (
    subject TEXT PRIMARY KEY, owner TEXT NOT NULL, created INTEGER NOT NULL DEFAULT 0,
    retired INTEGER NOT NULL DEFAULT 0, revision TEXT NOT NULL, winner BLOB NOT NULL,
    updated_at TEXT NOT NULL, changed_index INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS arrangements_owner_index ON arrangements(owner,subject);
CREATE INDEX IF NOT EXISTS arrangements_live_owner_index ON arrangements(owner,subject) WHERE created=1 AND retired=0;
CREATE INDEX IF NOT EXISTS arrangements_changed_index ON arrangements(changed_index);
CREATE INDEX IF NOT EXISTS arrangements_owner_changed_index ON arrangements(owner,changed_index);
CREATE TABLE IF NOT EXISTS arrangement_registers (
    subject TEXT NOT NULL, register TEXT NOT NULL, value TEXT NOT NULL,
    revision TEXT NOT NULL, winner BLOB NOT NULL,
    PRIMARY KEY(subject,register)
);
"#;

type Head = (Value, String, Vec<u8>);
fn heads(connection: &Connection, subject: &str) -> Result<BTreeMap<String, Head>> {
    let mut statement = connection.prepare_cached("SELECT register,value,revision,winner FROM arrangement_registers WHERE subject=?1 ORDER BY register")?;
    let rows = statement.query_map([subject], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|(register,value,revision,key)| Ok((register,(serde_json::from_str(&value)?,revision,key)))).collect()
}
fn register(head: &Head) -> Value { json!({"value":head.0,"revision":head.1}) }

pub(super) fn project(transaction: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
    let fields = schema_fields_for_body(&claim.kind, &claim.body)?;
    let operations = schema::operations(&claim.subject, &fields).map_err(anyhow::Error::new)?;
    let owner = schema::owner(&claim.subject).map_err(anyhow::Error::new)?;
    let key = canonical::sortable_key(&canonical::claim_key(transaction, &claim.id)?);
    transaction.execute("INSERT INTO arrangements(subject,owner,revision,winner,updated_at,changed_index) VALUES(?1,?2,?3,?4,?5,?6)
        ON CONFLICT(subject) DO UPDATE SET revision=CASE WHEN excluded.winner>arrangements.winner THEN excluded.revision ELSE arrangements.revision END,
        updated_at=CASE WHEN excluded.winner>arrangements.winner THEN excluded.updated_at ELSE arrangements.updated_at END,
        winner=MAX(arrangements.winner,excluded.winner),changed_index=MAX(arrangements.changed_index,excluded.changed_index)", params![claim.subject,owner,claim.id,key,claim.accepted_at_unix_ms.to_string(),claim.store_index])?;
    let mut writes = Vec::new();
    for operation in operations {
        match operation {
            Operation::Create { name } => {
                transaction.execute("UPDATE arrangements SET created=1 WHERE subject=?1", [&claim.subject])?;
                writes.push(("name".into(), json!(name)));
            }
            Operation::Rename { name } => writes.push(("name".into(), json!(name))),
            Operation::FolderCreate { id,name,parent,key } => {
                writes.push((format!("folder/{id}/name"), json!(name)));
                writes.push((format!("folder/{id}/position"), json!({"parent":parent,"key":key})));
            }
            Operation::FolderRename { id,name } => writes.push((format!("folder/{id}/name"), json!(name))),
            Operation::FolderMove { id,parent,key } => writes.push((format!("folder/{id}/position"), json!({"parent":parent,"key":key}))),
            Operation::FolderDelete { id } => writes.push((format!("folder/{id}/tombstone"), json!(true))),
            Operation::SubjectPlace { subject,folder,key } => writes.push((format!("placement/{subject}"), json!({"folder":folder,"key":key}))),
            Operation::Retire {} => { transaction.execute("UPDATE arrangements SET retired=1 WHERE subject=?1", [&claim.subject])?; }
        }
    }
    for (register,value) in writes {
        transaction.execute("INSERT INTO arrangement_registers(subject,register,value,revision,winner) VALUES(?1,?2,?3,?4,?5)
            ON CONFLICT(subject,register) DO UPDATE SET value=excluded.value,revision=excluded.revision,winner=excluded.winner
            WHERE excluded.winner>arrangement_registers.winner", params![claim.subject,register,canonical_json_text(&value)?,claim.id,key])?;
    }
    Ok(())
}

/// Cut cycles in raw positions first (including tombstoned folders), then lift through deleted
/// ancestors. Raw registers stay intact; missing folders and orphan roster subjects stay durable.
fn resolved(heads: &BTreeMap<String, Head>) -> Value {
    let mut positions = BTreeMap::<String, (Option<String>, Vec<u8>)>::new();
    let mut deleted = BTreeSet::new();
    for (register,head) in heads {
        if let Some(tail) = register.strip_prefix("folder/") {
            if let Some(id) = tail.strip_suffix("/position") { positions.insert(id.into(), (head.0["parent"].as_str().map(str::to_owned),head.2.clone())); }
            if let Some(id) = tail.strip_suffix("/tombstone") { deleted.insert(id.to_owned()); }
        }
    }
    let mut visited = BTreeSet::new();
    for start in positions.keys().cloned().collect::<Vec<_>>() {
        let mut path = Vec::<String>::new();
        let mut offsets = BTreeMap::new();
        let mut cursor = Some(start);
        while let Some(id) = cursor {
            if visited.contains(&id) { break; }
            if let Some(&offset) = offsets.get(&id) {
                let cut = path[offset..].iter().max_by(|a,b| positions[*a].1.cmp(&positions[*b].1).then_with(|| a.cmp(b))).expect("cycle is nonempty").clone();
                positions.get_mut(&cut).expect("cycle position exists").0 = None;
                break;
            }
            let Some((parent,_)) = positions.get(&id) else { break; };
            offsets.insert(id.clone(), path.len());
            path.push(id);
            cursor = parent.clone();
        }
        visited.extend(path);
    }
    let mut ancestors = BTreeMap::<String, Option<String>>::new();
    for start in positions.keys() {
        let mut path = Vec::new();
        let mut cursor = Some(start.clone());
        let result = loop {
            let Some(id) = cursor else { break None; };
            if let Some(known) = ancestors.get(&id) { break known.clone(); }
            let Some((parent,_)) = positions.get(&id) else { break None; };
            path.push(id.clone());
            if !heads.contains_key(&format!("folder/{id}/name")) { break None; }
            if !deleted.contains(&id) { break Some(id); }
            cursor = parent.clone();
        };
        for id in path { ancestors.insert(id,result.clone()); }
    }
    let live_ancestor = |id: Option<&str>| id.and_then(|id| ancestors.get(id)).cloned().flatten();
    let parents: BTreeMap<_,_> = positions.iter().map(|(id,(parent,_))| (id.clone(),live_ancestor(parent.as_deref()))).collect();
    let folders: BTreeMap<_,_> = heads.iter().filter_map(|(register,head)| register.strip_prefix("placement/").map(|subject| (subject.to_owned(),live_ancestor(head.0["folder"].as_str())))).collect();
    json!({"parents":parents,"folders":folders})
}

pub(super) fn arrangement_at(connection: &Connection, subject: &str, through: u64) -> Result<Option<Value>> {
    let row = connection.query_row("SELECT owner,created,retired,revision,updated_at,changed_index FROM arrangements WHERE subject=?1", [subject], |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?,row.get::<_,bool>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,u64>(5)?))).optional()?;
    let Some((owner,created,retired,revision,time,index)) = row else { return Ok(None); };
    anyhow::ensure!(index <= through, "arrangement snapshot frontier is stale; read current heads in a read snapshot");
    if !created || retired { return Ok(None); }
    let heads = heads(connection, subject)?;
    resource(subject, &owner, &revision, time.parse()?, &heads).map(Some)
}

fn resource(subject: &str, owner: &str, revision: &str, time: u128, heads: &BTreeMap<String, Head>) -> Result<Value> {
    let mut folders = serde_json::Map::new();
    let mut placements = serde_json::Map::new();
    for (key,head) in heads {
        if let Some(id) = key.strip_prefix("folder/").and_then(|tail| tail.strip_suffix("/name")) {
            if let Some(position) = heads.get(&format!("folder/{id}/position")) {
                folders.insert(id.into(),json!({"name":register(head),"position":register(position),"tombstone":heads.get(&format!("folder/{id}/tombstone")).map(register)}));
            }
        }
        if let Some(subject) = key.strip_prefix("placement/") { placements.insert(subject.into(),register(head)); }
    }
    Ok(json!({"id":subject,"kind":"arrangement","owner":owner,"revision":revision,"deleted":false,
        "body":{"version":1,"name":register(heads.get("name").context("created arrangement name")?),"folders":folders,"placements":placements},"resolved":resolved(heads),
        "updated_at":chrono::DateTime::from_timestamp_millis(i64::try_from(time).unwrap_or(i64::MAX)).unwrap_or(chrono::DateTime::UNIX_EPOCH).to_rfc3339_opts(chrono::SecondsFormat::Millis,true)}))
}
pub(super) fn arrangements_at(connection: &Connection, person: &str, through: u64) -> Result<Vec<Value>> {
    let changed: u64 = connection.query_row("SELECT COALESCE(MAX(changed_index),0) FROM arrangements WHERE owner=?1", [person], |row| row.get(0))?;
    anyhow::ensure!(changed <= through, "arrangement collection snapshot frontier is stale; read current heads in a read snapshot");
    let mut statement = connection.prepare_cached("SELECT subject FROM arrangements WHERE owner=?1 AND created=1 AND retired=0 ORDER BY subject")?;
    let subjects = statement.query_map([person], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    subjects.into_iter().filter_map(|subject| arrangement_at(connection,&subject,through).transpose()).collect()
}

pub(super) fn prepare(transaction: &Transaction<'_>, input: &ClaimInput) -> Result<(), St3Error> {
    if input.kind != "arrangement.edited" { return Ok(()); }
    let operations = schema::operations(&input.subject,&input.fields).map_err(|e| St3Error::new(e.code,e.message))?;
    let state = transaction.query_row("SELECT created,retired FROM arrangements WHERE subject=?1", [&input.subject], |row| Ok((row.get::<_,bool>(0)?,row.get::<_,bool>(1)?))).optional().map_err(internal)?;
    if state.is_some_and(|(_,retired)| retired) { return Err(St3Error::new("arrangement-retired","a retired arrangement ID cannot be reused")); }
    let creating = operations.iter().any(|op| matches!(op,Operation::Create { .. }));
    if creating == state.is_some_and(|(created,_)| created) { return Err(St3Error::new(if creating { "arrangement-exists" } else { "not-found" }, "creation requires a new arrangement; other edits require an existing arrangement")); }
    if creating {
        let count: usize = transaction.query_row("SELECT COUNT(*) FROM arrangements WHERE owner=?1 AND created=1 AND retired=0", [schema::owner(&input.subject).map_err(|e| St3Error::new(e.code,e.message))?], |row| row.get(0)).map_err(internal)?;
        if count >= schema::MAX_ARRANGEMENTS { return Err(St3Error::new("arrangement-limit","at most 100 arrangements may be live locally")); }
    }
    if matches!(operations.as_slice(), [Operation::Retire {}]) { return Ok(()); }
    let prepared_head = |value| (value, "0".repeat(64), Vec::new());
    let mut heads = heads(transaction,&input.subject).map_err(internal)?;
    let known_ids: BTreeSet<_> = heads.keys().filter_map(|key| key.strip_prefix("folder/").and_then(|tail| tail.split_once('/')).map(|(id,_)| id.to_owned())).collect();
    let initial_deleted: BTreeSet<_> = heads.keys().filter_map(|key| key.strip_prefix("folder/").and_then(|tail| tail.strip_suffix("/tombstone")).map(str::to_owned)).collect();
    let created_ids: BTreeSet<_> = operations.iter().filter_map(|op| if let Operation::FolderCreate { id,.. } = op { Some(id.as_str()) } else { None }).collect();
    // Evaluate the whole atomic edit's proposed raw positions, so moving a child out and its
    // former parent into that child in one rekey operation is valid, independent of op order.
    for op in &operations {
        match op {
            Operation::Create { name } | Operation::Rename { name } => { heads.insert("name".into(), prepared_head(json!(name))); }
            Operation::FolderCreate { id,name,parent,key } => {
                if known_ids.contains(id) { return Err(St3Error::new("arrangement-folder-exists","folder IDs cannot be reused")); }
                heads.insert(format!("folder/{id}/name"),prepared_head(json!(name)));
                heads.insert(format!("folder/{id}/position"),prepared_head(json!({"parent":parent,"key":key})));
            }
            Operation::FolderRename { id,.. } | Operation::FolderMove { id,.. } | Operation::FolderDelete { id } => {
                if !known_ids.contains(id) && !created_ids.contains(id.as_str()) { return Err(St3Error::new("not-found","the folder does not exist")); }
                if initial_deleted.contains(id) { return Err(St3Error::new("arrangement-folder-deleted","a deleted folder ID cannot be edited or reused")); }
                match op {
                    Operation::FolderRename { name,.. } => { heads.insert(format!("folder/{id}/name"),prepared_head(json!(name))); }
                    Operation::FolderMove { parent,key,.. } => { heads.insert(format!("folder/{id}/position"),prepared_head(json!({"parent":parent,"key":key}))); }
                    Operation::FolderDelete { .. } => { heads.insert(format!("folder/{id}/tombstone"),prepared_head(json!(true))); }
                    _ => unreachable!("folder mutation"),
                }
            }
            Operation::SubjectPlace { subject,folder,key } => { heads.insert(format!("placement/{subject}"),prepared_head(json!({"folder":folder,"key":key}))); }
            _ => {}
        }
    }
    for op in &operations {
        if let Operation::FolderCreate { id,parent,.. } | Operation::FolderMove { id,parent,.. } = op {
            let mut cursor = parent.as_deref();
            let mut seen = BTreeSet::new();
            while let Some(ancestor) = cursor {
                if ancestor == id { return Err(St3Error::new("arrangement-cycle","a folder cannot move into itself or its descendants")); }
                if !seen.insert(ancestor) { break; }
                cursor = heads.get(&format!("folder/{ancestor}/position")).and_then(|h| h.0["parent"].as_str());
            }
        }
    }
    let folders = heads.keys().filter(|k| k.starts_with("folder/") && k.ends_with("/name")).count();
    let placements = heads.keys().filter(|k| k.starts_with("placement/")).count();
    if folders > schema::MAX_FOLDERS || placements > schema::MAX_PLACEMENTS { return Err(St3Error::new("arrangement-limit","arrangement folder or placement count exceeds its admission bound")); }
    let projected = resource(&input.subject, input.fields["owner"].as_str().expect("validated owner"), &"0".repeat(64), now_ms(), &heads).map_err(internal)?;
    if serde_json::to_vec(&projected).map_err(internal)?.len() > schema::MAX_RESOURCE_BYTES { return Err(St3Error::new("arrangement-body-too-large","projected arrangement resource exceeds 512 KiB")); }
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let ready: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='arrangement_heads_v1')", [], |row| row.get(0))?;
    if !ready { rebuild(transaction)?; transaction.execute("INSERT OR REPLACE INTO meta(key,value) VALUES('arrangement_heads_v1','1')", [])?; }
    Ok(())
}
pub(super) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute("DELETE FROM arrangement_registers", [])?;
    transaction.execute("DELETE FROM arrangements", [])?;
    let mut statement = transaction.prepare("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE kind='arrangement.edited' AND NOT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=claims.id AND state='repaired')")?;
    let claims = statement.query_map([],claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for claim in claims { project(transaction,&claim)?; }
    Ok(())
}
impl Store {
    pub fn arrangements(&self, person: &str, through: u64) -> Result<Vec<Value>> {
        self.read_snapshot(|_| arrangements_at(&self.readers.get(),person,through))
    }
    pub fn arrangement(&self, subject: &str, through: u64) -> Result<Option<Value>> {
        self.read_snapshot(|_| arrangement_at(&self.readers.get(),subject,through))
    }
    pub(crate) fn arrangements_changed(&self, after: u64, through: u64) -> Result<bool> {
        Ok(self.readers.get().query_row("SELECT EXISTS(SELECT 1 FROM arrangements WHERE changed_index>?1 AND changed_index<=?2)",params![after,through.min(i64::MAX as u64)],|row| row.get(0))?)
    }
    pub fn edit_arrangement(&self, input: &ClaimInput, expected_subjects: &BTreeMap<String,String>) -> Result<ClaimRecord,St3Error> {
        if input.kind != "arrangement.edited" { return Err(St3Error::new("invalid-arrangement-operations","edit_arrangement requires arrangement.edited")); }
        if let Some(actor) = &input.actor { self.graph.ensure_principal_key(actor)?; }
        if input.expected_subject.is_some() {
            let mut merging = input.clone();
            merging.expected_subject = None;
            return append_claim_with_subject_fences(&self.graph,&merging,None,None,Some(expected_subjects)).map(|(claim,_)| claim);
        }
        append_claim_with_subject_fences(&self.graph,input,None,None,Some(expected_subjects)).map(|(claim,_)| claim)
    }
}

//! Claim-backed pair records and their lifecycle-aware, keyset-addressable live ordering.
//! Hidden and absent winners remain authority; the existing reverse edge index drives fanout.
use super::*;
use st3_schema::lifecycle::{self, EffectiveBucket, VisibilityPolicy};
use st3_schema::ordered_membership::{self as schema, Operation, Position};

pub(super) const MAX_LIVE_MEMBERS: usize = 4096;
const VERSION_KEY: &str = "ordered_membership_heads_v1";
pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ordered_membership_heads (
    container TEXT NOT NULL, member TEXT NOT NULL, position TEXT,
    revision TEXT NOT NULL, winner BLOB NOT NULL,
    PRIMARY KEY(container,member)
);
CREATE TABLE IF NOT EXISTS ordered_membership_live (
    container TEXT NOT NULL, bucket TEXT NOT NULL, key TEXT NOT NULL,
    member TEXT NOT NULL, revision TEXT NOT NULL,
    PRIMARY KEY(container,bucket,key,member), UNIQUE(container,member)
);
CREATE TABLE IF NOT EXISTS ordered_membership_counts (
    container TEXT PRIMARY KEY, live_count INTEGER NOT NULL DEFAULT 0 CHECK(live_count>=0),
    changed_index INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ordered_membership_counts_changed ON ordered_membership_counts(changed_index);
CREATE TABLE IF NOT EXISTS ordered_membership_lifecycle (
    subject TEXT PRIMARY KEY, visible INTEGER NOT NULL CHECK(visible IN (0,1)), revision TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS local_ordered_membership_pending (subject TEXT PRIMARY KEY);
"#;

/// Invalidation sources are selected by the schema-adjacent registry, not member kind branches.
pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    let mut sources = BTreeMap::<_, Vec<_>>::new();
    for entry in lifecycle::registry().entries() {
        let source = match entry.visibility {
            VisibilityPolicy::DesiredDeclaration => ("desired", "subject"),
            VisibilityPolicy::MissionDeclaration => ("mission_definitions", "'mission/' || {row}.mission_id"),
            VisibilityPolicy::Arrangement => ("arrangements", "subject"),
            VisibilityPolicy::Glass => ("glass_heads", "subject"),
            VisibilityPolicy::Document => ("documents", "name"),
            VisibilityPolicy::RetainedClaim => ("claims", "subject"),
        };
        sources.entry(source).or_default().push(entry.family);
    }
    for ((table, identity), families) in sources {
        for (event, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
            let identity = if identity.contains("{row}") { identity.replace("{row}", row) } else { format!("{row}.{identity}") };
            let filter = if table == "claims" {
                families.iter().map(|family| format!("{identity} LIKE '{family}/%'")).collect::<Vec<_>>().join(" OR ")
            } else { "1".into() };
            connection.execute_batch(&format!(
                "DROP TRIGGER IF EXISTS ordered_membership_{table}_{event};
                 CREATE TRIGGER ordered_membership_{table}_{event} AFTER {event} ON {table} WHEN ({filter}) BEGIN
                 INSERT INTO local_ordered_membership_pending(subject)
                 SELECT {identity} WHERE EXISTS(SELECT 1 FROM declared_resource_edges
                     WHERE target={identity} AND relation='ordered-membership')
                 ON CONFLICT(subject) DO NOTHING; END;"
            ))?;
        }
    }
    Ok(())
}

fn lifecycle_at(connection: &Connection, subject: &str) -> Result<(bool, String)> {
    let Some(entry) = lifecycle::registry().subject(subject) else { return Ok((false, String::new())); };
    let state: Option<(bool, String)> = match entry.visibility {
        VisibilityPolicy::DesiredDeclaration => connection.query_row(
            "SELECT kind!='stop',claim_id FROM desired WHERE subject=?1", [subject],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
        VisibilityPolicy::MissionDeclaration => connection.query_row(
            "SELECT state!='retired',claim_id FROM mission_definitions WHERE mission_id=?1",
            [subject.strip_prefix("mission/").unwrap_or(subject)], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
        VisibilityPolicy::Arrangement => connection.query_row(
            "SELECT created=1 AND retired=0,revision FROM arrangements WHERE subject=?1", [subject],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
        VisibilityPolicy::Glass => connection.query_row(
            "SELECT deleted=0 AND claim_id IS NOT NULL,COALESCE(claim_id,'') FROM glass_heads WHERE subject=?1", [subject],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
        VisibilityPolicy::Document => connection.query_row(
            "SELECT 1,binding_claim_id FROM documents WHERE name=?1 ORDER BY binding_key DESC,hash DESC LIMIT 1",
            [subject], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
        VisibilityPolicy::RetainedClaim => connection.query_row(
            &canonical_sql("SELECT id FROM claims WHERE subject=?1 AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id) ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            [subject], |row| row.get::<_,String>(0),
        ).optional()?.map(|revision| (true, revision)),
    };
    Ok(state.unwrap_or((false, String::new())))
}

fn remember_lifecycle(transaction: &Transaction<'_>, subject: &str) -> Result<bool> {
    let (visible, revision) = lifecycle_at(transaction, subject)?;
    transaction.execute(
        "INSERT INTO ordered_membership_lifecycle(subject,visible,revision) VALUES(?1,?2,?3)
         ON CONFLICT(subject) DO UPDATE SET visible=excluded.visible,revision=excluded.revision
         WHERE visible!=excluded.visible OR revision!=excluded.revision", params![subject, visible, revision],
    )?;
    Ok(visible)
}

fn container_live(connection: &Connection, container: &str) -> Result<bool> {
    let Some(entry) = lifecycle::registry().subject(container) else { return Ok(false); };
    let Some(capability) = &entry.ordered_membership else { return Ok(false); };
    match capability.effective_bucket {
        EffectiveBucket::ArrangementFolders { layout_version } => Ok(lifecycle_at(connection, container)?.0 && arrangements::version(connection, container)? == layout_version),
    }
}

struct ContainerView {
    live: bool,
    buckets: BTreeMap<String, Option<String>>,
}

fn container_view(transaction: &Transaction<'_>, container: &str) -> Result<ContainerView> {
    let live = container_live(transaction, container)?;
    remember_lifecycle(transaction, container)?;
    let buckets = if live {
        let capability = lifecycle::registry().subject(container).and_then(|entry| entry.ordered_membership.as_ref())
            .context("ordered membership container capability")?;
        match capability.effective_bucket {
            EffectiveBucket::ArrangementFolders { .. } => arrangements::effective_buckets(transaction, container)?,
        }
    } else { BTreeMap::new() };
    Ok(ContainerView { live, buckets })
}

fn refresh_pair(transaction: &Transaction<'_>, container: &str, member: &str, index: u64, view: &ContainerView) -> Result<()> {
    let row: Option<(Option<String>, String)> = transaction.query_row(
        "SELECT position,revision FROM ordered_membership_heads WHERE container=?1 AND member=?2",
        params![container, member], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let previous: Option<(String, String, String)> = transaction.query_row(
        "SELECT bucket,key,revision FROM ordered_membership_live WHERE container=?1 AND member=?2",
        params![container, member], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let member_live = remember_lifecycle(transaction, member)?;
    let next = if member_live && view.live {
        row.and_then(|(position, revision)| position.map(|position| (position, revision)))
            .map(|(position, revision)| -> Result<_> {
                let position: Position = serde_json::from_str(&position)?;
                Ok((position.bucket.as_deref().and_then(|bucket| view.buckets.get(bucket)).cloned().flatten().unwrap_or_default(), position.key, revision))
            }).transpose()?
    } else { None };
    transaction.execute(
        "INSERT OR IGNORE INTO ordered_membership_counts(container,changed_index) VALUES(?1,?2)", params![container,index],
    )?;
    if previous != next {
        transaction.execute("DELETE FROM ordered_membership_live WHERE container=?1 AND member=?2", params![container,member])?;
        if let Some((bucket, key, revision)) = &next {
            transaction.execute("INSERT INTO ordered_membership_live(container,bucket,key,member,revision) VALUES(?1,?2,?3,?4,?5)", params![container,bucket,key,member,revision])?;
        }
        let delta = i64::from(next.is_some()) - i64::from(previous.is_some());
        transaction.execute("UPDATE ordered_membership_counts SET live_count=live_count+?2,changed_index=MAX(changed_index,?3) WHERE container=?1", params![container,delta,index])?;
    }
    Ok(())
}

pub(super) fn container_changed(transaction: &Transaction<'_>, container: &str, index: u64) -> Result<()> {
    let mut statement = transaction.prepare_cached("SELECT member FROM ordered_membership_heads WHERE container=?1 ORDER BY member")?;
    let members = statement.query_map([container], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if !members.is_empty() {
        let view = container_view(transaction, container)?;
        for member in &members { refresh_pair(transaction, container, member, index, &view)?; }
    } else if arrangements::migrated(transaction, container)? {
        // Migration retains a container lifecycle dependency even with zero pairs.
        // Keep it current on name/folder edits and retirement, exactly as replay does.
        remember_lifecycle(transaction, container)?;
    }
    if arrangements::version(transaction, container)? == 2 {
        transaction.execute(
            "INSERT INTO ordered_membership_counts(container,changed_index) VALUES(?1,?2)
             ON CONFLICT(container) DO UPDATE SET changed_index=MAX(changed_index,excluded.changed_index)",
            params![container,index],
        )?;
    } else if members.is_empty() {
        // A delayed legacy create can deactivate a previously observed v2 create.
        // Keep count rows for retained raw pairs, but not for an inactive empty container.
        transaction.execute("DELETE FROM ordered_membership_counts WHERE container=?1", [container])?;
    }
    Ok(())
}

/// Flush lifecycle-only changes in the same writer transaction as the selected declaration.
pub(super) fn flush(transaction: &Transaction<'_>) -> Result<()> {
    let mut statement = transaction.prepare_cached("SELECT subject FROM local_ordered_membership_pending ORDER BY subject")?;
    let subjects = statement.query_map([], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if subjects.is_empty() { return Ok(()); }
    let index = current_index(transaction)?;
    transaction.execute("DELETE FROM local_ordered_membership_pending", [])?;
    let mut views = BTreeMap::new();
    for subject in subjects {
        let mut statement = transaction.prepare_cached(
            "SELECT owner FROM declared_resource_edges WHERE target=?1 AND relation='ordered-membership' ORDER BY owner,name",
        )?;
        let containers = statement.query_map([&subject], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for container in containers {
            if !views.contains_key(&container) { views.insert(container.clone(), container_view(transaction, &container)?); }
            refresh_pair(transaction, &container, &subject, index, &views[&container])?;
        }
    }
    Ok(())
}

/// A repair can remove the final count row. Keep its invalidation watermark locally,
/// independent of rebuildable shared rows and their surviving claims' arrival indexes.
pub(super) fn repair_frontier(transaction: &Transaction<'_>, subject: &str) -> Result<()> {
    let index = current_index(transaction)?;
    transaction.execute(
        "INSERT INTO meta(key,value) VALUES('local_ordered_membership_repair_index',?1)
         ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)",
        [index],
    )?;
    transaction.execute(
        "UPDATE ordered_membership_counts SET changed_index=MAX(changed_index,?2)
         WHERE container=?1 OR container IN
           (SELECT owner FROM declared_resource_edges WHERE target=?1 AND relation='ordered-membership')",
        params![subject,index],
    )?;
    Ok(())
}

/// Retained-claim visibility is selected directly from unrepaired claim authority.
/// Repair changes that selection without updating a claims row or firing its triggers.
pub(super) fn member_repaired(transaction: &Transaction<'_>, subject: &str) -> Result<()> {
    if !matches!(lifecycle::registry().subject(subject).map(|entry| entry.visibility),
        Some(VisibilityPolicy::RetainedClaim)) { return Ok(()); }
    let referenced: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM declared_resource_edges WHERE target=?1 AND relation='ordered-membership')",
        [subject], |row| row.get(0),
    )?;
    if !referenced { return Ok(()); }
    transaction.execute(
        "INSERT INTO local_ordered_membership_pending(subject) VALUES(?1) ON CONFLICT(subject) DO NOTHING",
        [subject],
    )?;
    flush(transaction)?;
    repair_frontier(transaction, subject)
}

/// Historical placement claims keep their original canonical key, revision and real actor.
/// This boundary translates values only; it never creates a replacement authority claim.
fn legacy_head(transaction: &Transaction<'_>, container: &str, member: &str,
    placement: &Value, revision: &str, winner: &[u8], index: u64, view: &ContainerView) -> Result<()> {
    let position = canonical_serialized_json_text(&Position {
        bucket: placement["folder"].as_str().map(str::to_owned),
        key: placement["key"].as_str().context("historical placement key")?.to_owned(),
    })?;
    let changed = transaction.execute(
        "INSERT INTO ordered_membership_heads(container,member,position,revision,winner) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(container,member) DO UPDATE SET position=excluded.position,revision=excluded.revision,winner=excluded.winner
         WHERE excluded.winner>ordered_membership_heads.winner", params![container,member,position,revision,winner],
    )?;
    transaction.execute("INSERT OR IGNORE INTO declared_resource_edges(owner,relation,name,target)
        VALUES(?1,'ordered-membership',?2,?2)", params![container,member])?;
    if changed != 0 { refresh_pair(transaction, container, member, index, view)?; }
    Ok(())
}

pub(super) fn backfill_legacy(transaction: &Transaction<'_>, container: &str, index: u64) -> Result<()> {
    let view = container_view(transaction, container)?;
    let mut statement = transaction.prepare_cached(
        "SELECT substr(register,11),value,revision,winner FROM arrangement_registers
         WHERE subject=?1 AND register LIKE 'placement/%' ORDER BY register")?;
    let rows = statement.query_map([container], |row| Ok((row.get::<_,String>(0)?,
        row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,Vec<u8>>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (member, value, revision, winner) in rows {
        legacy_head(transaction, container, &member, &serde_json::from_str(&value)?,
            &revision, &winner, index, &view)?;
    }
    Ok(())
}

pub(super) fn project_legacy_claim(transaction: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
    if !arrangements::migrated(transaction, &claim.subject)? { return Ok(()); }
    let fields = schema_fields_for_body(&claim.kind, &claim.body)?;
    let operations = st3_schema::arrangements::operations(&claim.subject, &fields).map_err(anyhow::Error::new)?;
    let winner = canonical::sortable_key(&canonical::claim_key(transaction, &claim.id)?);
    let view = container_view(transaction, &claim.subject)?;
    for operation in operations {
        if let st3_schema::arrangements::Operation::SubjectPlace { subject, folder, key } = operation {
            legacy_head(transaction, &claim.subject, &subject, &json!({"folder":folder,"key":key}),
                &claim.id, &winner, claim.store_index, &view)?;
        }
    }
    Ok(())
}

pub(super) fn project(transaction: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
    let repaired: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=?1)",
        [&claim.id], |row| row.get(0),
    )?;
    if repaired { return Ok(()); }
    let fields = schema_fields_for_body(&claim.kind, &claim.body)?;
    let operations = schema::operations(&claim.subject, &fields).map_err(anyhow::Error::new)?;
    let winner = canonical::sortable_key(&canonical::claim_key(transaction, &claim.id)?);
    let view = container_view(transaction, &claim.subject)?;
    for operation in operations {
        let (member, position) = match operation {
            Operation::Place { member, bucket, key } => (member, Some(canonical_serialized_json_text(&Position { bucket,key })?)),
            Operation::Remove { member } => (member, None),
        };
        let changed = transaction.execute(
            "INSERT INTO ordered_membership_heads(container,member,position,revision,winner) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(container,member) DO UPDATE SET position=excluded.position,revision=excluded.revision,winner=excluded.winner
             WHERE excluded.winner>ordered_membership_heads.winner", params![claim.subject,member,position,claim.id,winner],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO declared_resource_edges(owner,relation,name,target) VALUES(?1,'ordered-membership',?2,?2)", params![claim.subject,member],
        )?;
        if changed != 0 { refresh_pair(transaction, &claim.subject, &member, claim.store_index, &view)?; }
    }
    // A replica can learn pair claims before the container's create claim. Keep a pending
    // arrangement head (created=0), so later creation cannot lose the pair's newer revision.
    transaction.execute(
        "INSERT INTO arrangements(subject,owner,revision,winner,updated_at,changed_index) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(subject) DO UPDATE SET
         revision=CASE WHEN excluded.winner>arrangements.winner THEN excluded.revision ELSE arrangements.revision END,
         updated_at=CASE WHEN excluded.winner>arrangements.winner THEN excluded.updated_at ELSE arrangements.updated_at END,
         winner=MAX(arrangements.winner,excluded.winner),changed_index=MAX(arrangements.changed_index,excluded.changed_index)",
        params![claim.subject,fields["owner"].as_str().context("validated membership owner")?,claim.id,winner,claim.accepted_at_unix_ms.to_string(),claim.store_index],
    )?;
    remember_lifecycle(transaction, &claim.subject)?;
    transaction.execute(
        "INSERT INTO ordered_membership_counts(container,changed_index) VALUES(?1,?2)
         ON CONFLICT(container) DO UPDATE SET changed_index=MAX(changed_index,excluded.changed_index)", params![claim.subject,claim.store_index],
    )?;
    Ok(())
}

pub(super) fn prepare(transaction: &Transaction<'_>, input: &ClaimInput) -> Result<(), St3Error> {
    if input.kind != "ordered-membership.edited" { return Ok(()); }
    // Selected declaration changes earlier in this writer batch must affect admission too.
    flush(transaction).map_err(internal)?;
    let operations = schema::operations(&input.subject, &input.fields).map_err(|error| St3Error::new(error.code,error.message))?;
    if !container_live(transaction, &input.subject).map_err(internal)? {
        return Err(St3Error::new("invalid-arrangement-operations", "ordered memberships require a live version-2 arrangement; existing version-1 placements require explicit migration"));
    }
    let mut count: i64 = transaction.query_row(
        "SELECT COALESCE((SELECT live_count FROM ordered_membership_counts WHERE container=?1),0)", [&input.subject], |row| row.get(0),
    ).map_err(internal)?;
    for operation in operations {
        let (member, next_present) = match operation {
            Operation::Place { member,.. } => (member,true),
            Operation::Remove { member } => (member,false),
        };
        let previous: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM ordered_membership_live WHERE container=?1 AND member=?2)", params![input.subject,member], |row| row.get(0),
        ).map_err(internal)?;
        let next = next_present && lifecycle_at(transaction, &member).map_err(internal)?.0;
        count += i64::from(next) - i64::from(previous);
    }
    if count > MAX_LIVE_MEMBERS as i64 {
        return Err(St3Error::new("arrangement-limit", "ordered membership admission exceeds 4096 live entries; hidden lifetime pairs do not count"));
    }
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let ready: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)", [VERSION_KEY], |row| row.get(0))?;
    if !ready { rebuild(transaction)?; transaction.execute("INSERT OR REPLACE INTO meta(key,value) VALUES(?1,'1')", [VERSION_KEY])?; }
    Ok(())
}

pub(super) fn rebuild(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch("DELETE FROM ordered_membership_live; DELETE FROM ordered_membership_heads;
        DELETE FROM ordered_membership_counts; DELETE FROM ordered_membership_lifecycle;
        DELETE FROM local_ordered_membership_pending; DELETE FROM declared_resource_edges WHERE relation='ordered-membership';")?;
    let mut statement = transaction.prepare("SELECT a.subject,a.changed_index FROM arrangements a
        WHERE EXISTS(SELECT 1 FROM arrangement_registers r WHERE r.subject=a.subject
            AND ((r.register='version' AND r.value='2') OR r.register='membership-authority'))")?;
    let containers = statement.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (container, index) in containers {
        if arrangements::version(transaction, &container)? == 2 {
            transaction.execute("INSERT INTO ordered_membership_counts(container,changed_index) VALUES(?1,?2)", params![container,index])?;
        }
    }
    let mut statement = transaction.prepare("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims
        WHERE kind IN ('ordered-membership.edited','arrangement.edited') AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=claims.id)")?;
    let claims = statement.query_map([],claim_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for claim in claims {
        if claim.kind == "arrangement.edited" { project_legacy_claim(transaction, &claim)?; }
        else { project(transaction, &claim)?; }
    }
    flush(transaction)?;
    Ok(())
}

/// Checkpoint answers include hidden/absent winners and their cross-subject dependencies.
pub(super) fn witness(connection: &Connection, container: &str) -> Result<Value> {
    let mut statement = connection.prepare_cached(
        "SELECT member,position,revision,hex(winner) FROM ordered_membership_heads WHERE container=?1 ORDER BY member",
    )?;
    let raw = statement.query_map([container], |row| {
        Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(member,position,revision,winner)| -> Result<Value> {
        Ok(json!({"member":member,"position":position.map(|text| serde_json::from_str::<Value>(&text)).transpose()?,"revision":revision,"winner":winner}))
    }).collect::<Result<Vec<_>>>()?;
    let mut statement = connection.prepare_cached(
        "SELECT bucket,key,member,revision FROM ordered_membership_live WHERE container=?1 ORDER BY bucket,key,member",
    )?;
    let live = statement.query_map([container], |row| {
        Ok(json!({"bucket":row.get::<_,String>(0)?,"key":row.get::<_,String>(1)?,"member":row.get::<_,String>(2)?,"revision":row.get::<_,String>(3)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut statement = connection.prepare_cached(
        "SELECT subject,visible,revision FROM ordered_membership_lifecycle WHERE subject=?1 OR subject IN
         (SELECT member FROM ordered_membership_heads WHERE container=?1) ORDER BY subject",
    )?;
    let dependencies = statement.query_map([container], |row| {
        Ok(json!({"subject":row.get::<_,String>(0)?,"visible":row.get::<_,bool>(1)?,"revision":row.get::<_,String>(2)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let count: u64 = connection.query_row(
        "SELECT COALESCE((SELECT live_count FROM ordered_membership_counts WHERE container=?1),0)", [container], |row| row.get(0),
    )?;
    Ok(json!({"raw":raw,"live":live,"count":count,"dependencies":dependencies}))
}

fn state_at(connection: &Connection, container: &str) -> Result<schema::State> {
    Ok(connection.query_row(
        "SELECT COALESCE(c.live_count,0),
           MAX(COALESCE(c.changed_index,0),COALESCE(CAST(r.value AS INTEGER),0))
         FROM (SELECT ?1 AS container) requested
         LEFT JOIN ordered_membership_counts c ON c.container=requested.container
         LEFT JOIN meta r ON r.key='local_ordered_membership_repair_index'",
        [container], |row| Ok(schema::State { live_count: row.get(0)?, changed_index: row.get(1)? }),
    )?)
}

pub(super) fn items_at(connection: &Connection, container: &str, through: u64, after: Option<(&str,&str,&str)>, limit: usize) -> Result<Vec<Value>> {
    anyhow::ensure!(limit <= 1001, "ordered membership pages are bounded to 1000 entries plus one lookahead");
    let created: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM arrangements WHERE subject=?1 AND created=1)", [container], |row| row.get(0))?;
    if !created { return Err(anyhow::Error::new(St3Error::new("not-found", "ordered membership container is unknown"))); }
    if arrangements::version(connection, container)? != 2 {
        return Err(anyhow::Error::new(St3Error::new("invalid-arrangement-operations", "ordered membership reads require version 2; version-1 placements are not an empty membership collection")));
    }
    anyhow::ensure!(state_at(connection, container)?.changed_index <= through,
        "ordered membership snapshot frontier is stale");
    let read_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        let member: String = row.get(2)?;
        let bucket: String = row.get(0)?;
        Ok(json!({"id":member,"container":container,"member":member,"position":{"bucket":if bucket.is_empty() {None} else {Some(bucket)},"key":row.get::<_,String>(1)?},"revision":row.get::<_,String>(3)?}))
    };
    if let Some((bucket,key,member)) = after {
        let mut statement = connection.prepare_cached(
            "SELECT bucket,key,member,revision FROM ordered_membership_live
             WHERE container=?1 AND (bucket,key,member)>(?2,?3,?4) ORDER BY bucket,key,member LIMIT ?5",
        )?;
        Ok(statement.query_map(params![container,bucket,key,member,limit as i64], read_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        let mut statement = connection.prepare_cached(
            "SELECT bucket,key,member,revision FROM ordered_membership_live WHERE container=?1 ORDER BY bucket,key,member LIMIT ?2",
        )?;
        Ok(statement.query_map(params![container,limit as i64], read_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Store {
    pub fn edit_ordered_memberships(&self, input: &ClaimInput, expected: &BTreeMap<String,String>) -> Result<ClaimRecord,St3Error> {
        if input.kind != "ordered-membership.edited" { return Err(St3Error::new("invalid-arrangement-operations", "membership edits require ordered-membership.edited")); }
        if let Some(actor) = &input.actor { self.graph.ensure_principal_key(actor)?; }
        append_claim_with_subject_fences(&self.graph,input,None,None,Some(expected),None).map(|(claim,_)| claim)
    }
    pub fn ordered_memberships(&self, container: &str, through: u64, after: Option<(&str,&str,&str)>, limit: usize) -> Result<Vec<Value>> {
        self.read_snapshot(|_| items_at(&self.readers.get(),container,through,after,limit))
    }
    pub fn ordered_membership_state(&self, container: &str) -> Result<schema::State> {
        self.read_snapshot(|_| state_at(&self.readers.get(),container))
    }
    pub fn ordered_membership_count(&self, container: &str) -> Result<u64> {
        Ok(self.readers.get().query_row("SELECT COALESCE((SELECT live_count FROM ordered_membership_counts WHERE container=?1),0)", [container], |row| row.get(0))?)
    }
    pub(crate) fn ordered_memberships_changed(&self, after: u64, through: u64) -> Result<bool> {
        Ok(self.readers.get().query_row(
            "SELECT EXISTS(SELECT 1 FROM ordered_membership_counts WHERE changed_index>?1 AND changed_index<=?2)
             OR EXISTS(SELECT 1 FROM meta WHERE key='local_ordered_membership_repair_index'
               AND CAST(value AS INTEGER)>?1 AND CAST(value AS INTEGER)<=?2)",
            params![after,through.min(i64::MAX as u64)], |row| row.get(0),
        )?)
    }
}

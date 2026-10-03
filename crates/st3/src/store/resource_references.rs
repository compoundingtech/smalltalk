//! Incoming declaration edges, derived transactionally from the selected declarations.
use super::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS declared_resource_edges (
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    target TEXT NOT NULL,
    reason TEXT,
    PRIMARY KEY(owner, name)
);
CREATE INDEX IF NOT EXISTS declared_resource_edges_target ON declared_resource_edges(target, owner, name);
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_insert AFTER INSERT ON desired BEGIN
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT NEW.subject, json_extract(value,'$.name'), json_extract(value,'$.subject'), json_extract(value,'$.reason')
    FROM json_each(NEW.body,'$.resources') WHERE NEW.kind='agent';
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_update AFTER UPDATE ON desired BEGIN
    DELETE FROM declared_resource_edges WHERE owner=OLD.subject;
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT NEW.subject, json_extract(value,'$.name'), json_extract(value,'$.subject'), json_extract(value,'$.reason')
    FROM json_each(NEW.body,'$.resources') WHERE NEW.kind='agent';
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_agent_delete AFTER DELETE ON desired BEGIN
    DELETE FROM declared_resource_edges WHERE owner=OLD.subject;
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_insert AFTER INSERT ON mission_definitions BEGIN
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT 'mission/' || NEW.mission_id, json_extract(edge.value,'$.name'), json_extract(edge.value,'$.subject'), json_extract(edge.value,'$.reason')
    FROM mission_revisions r, json_each(r.body,'$.resources') edge
    WHERE r.mission_id=NEW.mission_id AND r.revision=NEW.revision;
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_update AFTER UPDATE ON mission_definitions BEGIN
    DELETE FROM declared_resource_edges WHERE owner='mission/' || OLD.mission_id;
    INSERT INTO declared_resource_edges(owner, name, target, reason)
    SELECT 'mission/' || NEW.mission_id, json_extract(edge.value,'$.name'), json_extract(edge.value,'$.subject'), json_extract(edge.value,'$.reason')
    FROM mission_revisions r, json_each(r.body,'$.resources') edge
    WHERE r.mission_id=NEW.mission_id AND r.revision=NEW.revision;
END;
CREATE TRIGGER IF NOT EXISTS declared_resource_mission_delete AFTER DELETE ON mission_definitions BEGIN
    DELETE FROM declared_resource_edges WHERE owner='mission/' || OLD.mission_id;
END;
"#;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let initialized: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='declared_resource_edges_version' AND value='1')",
        [], |row| row.get(0),
    )?;
    if !initialized {
        transaction.execute_batch(
            "DELETE FROM declared_resource_edges;
             INSERT INTO declared_resource_edges(owner,name,target,reason)
             SELECT d.subject,json_extract(e.value,'$.name'),json_extract(e.value,'$.subject'),json_extract(e.value,'$.reason')
             FROM desired d,json_each(d.body,'$.resources') e WHERE d.kind='agent';
             INSERT INTO declared_resource_edges(owner,name,target,reason)
             SELECT 'mission/' || d.mission_id,json_extract(e.value,'$.name'),json_extract(e.value,'$.subject'),json_extract(e.value,'$.reason')
             FROM mission_definitions d JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision,
                  json_each(r.body,'$.resources') e;
             INSERT OR REPLACE INTO meta(key,value) VALUES('declared_resource_edges_version','1');"
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_resource_edges_follow_replacement_replication_and_replay() {
        let store = Store::open_memory("node").unwrap();
        let publish = |resources: &str, key: &str| {
            let kdl = format!(
                "version 2\nagent \"ada/seat\" {{ workspace \"/tmp\"; command \"true\"; {resources} }}\n\
                 mission \"ada/work\" state=\"ready\" {{ goal \"Work.\"; {resources} step \"work\" {{ }} }}\n"
            );
            let intent = crate::graph::parse_test_intent(&kdl, "node").unwrap();
            let preview = store.mission(&intent, IntentInput {
                kdl, source_name: None,
            }).unwrap();
            store.apply(&intent, &preview.subject_tokens, key).unwrap();
        };
        publish("resource \"goal\" uri=\"https://example.com/goal\" reason=\"shared goal\";", "first");
        let subject = format!("resource/uri/{}", hex::encode(Sha256::digest(b"https://example.com/goal")));
        let expected = json!([
            {"owner":"agent/ada/seat","name":"goal","reason":"shared goal"},
            {"owner":"mission/ada/work","name":"goal","reason":"shared goal"}
        ]);
        assert_eq!(json!(store.declared_resource_referrers(&subject).unwrap()), expected);
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            // Simulate opening a store written before this projection existed.
            transaction.execute("DELETE FROM declared_resource_edges", []).unwrap();
            transaction.execute("DELETE FROM meta WHERE key='declared_resource_edges_version'", []).unwrap();
            open(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(json!(store.declared_resource_referrers(&subject).unwrap()), expected);
        let replica = Store::open_memory("peer").unwrap();
        replica.import_replication("node", &store.export_replication(0).unwrap()).unwrap();
        assert_eq!(json!(replica.declared_resource_referrers(&subject).unwrap()), expected);
        {
            let mut connection = replica.connection.write();
            let transaction = connection.transaction().unwrap();
            replay_graph_from_nothing_tx(&transaction).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(json!(replica.declared_resource_referrers(&subject).unwrap()), expected);
        publish("", "removed");
        assert_eq!(store.declared_resource_referrers(&subject).unwrap(), Vec::<Value>::new());
        replica.import_replication("node", &store.export_replication(0).unwrap()).unwrap();
        assert_eq!(replica.declared_resource_referrers(&subject).unwrap(), Vec::<Value>::new());
    }
}

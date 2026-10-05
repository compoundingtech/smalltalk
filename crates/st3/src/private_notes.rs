//! Owner-local private notes: catalog-derived identity, filesystem byte authority,
//! and private durable mutation receipts. Nothing in this module enters fleet replication.
use crate::model::St3Error;
use crate::private_notes_fs as fs;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use st3_schema::private_notes::PrivateNotesUri;
use std::fs::File;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

const MAX_BYTES: usize = 1_048_576;

// Leave room for duplicated URI/fence metadata and the client snapshot envelope.
fn fits_client_response(markdown: &str) -> bool {
    markdown.len() <= MAX_BYTES && markdown.bytes().map(|byte| match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
        0..=31 => 6,
        _ => 1,
    }).sum::<usize>() <= MAX_BYTES - 16_384
}

#[derive(Clone, Debug, Default)]
pub struct Authority {
    pub person: Option<String>,
    pub catalogs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotesFence {
    pub carrier_generation: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotesWrite {
    pub uri: String,
    pub markdown: String,
    pub fence: NotesFence,
}

#[derive(Clone, Debug, Serialize)]
pub struct Notes {
    pub uri: String,
    pub markdown: String,
    pub fence: NotesFence,
}

struct Resolved {
    directory: File,
    generation: String,
}

fn refusal(code: &'static str, message: &str) -> St3Error { St3Error::new(code, message) }
fn io(error: std::io::Error) -> St3Error {
    let code = if error.kind() == std::io::ErrorKind::PermissionDenied { "forbidden" } else { "private-notes-unreachable" };
    // Filesystem error strings can reveal private realization paths; report only the class.
    refusal(code, "the owner-local notes source or carrier could not be accessed safely")
}
fn database_error(_: rusqlite::Error) -> St3Error {
    refusal("private-notes-unreachable", "the private notes recovery ledger is unavailable")
}
fn digest(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

impl Authority {
    fn resolve(&self, node: &str, uri: &PrivateNotesUri) -> Result<Resolved, St3Error> {
        if uri.host() != node {
            return Err(refusal("forbidden", "private notes belong to a different owner node"));
        }
        let mut resolved: Option<Resolved> = None;
        for root in &self.catalogs {
            let catalog = fs::open_directory(root).map_err(io)?;
            let agent_path = root.join("agents").join(uri.host()).join(uri.identity()).join("agent.kdl");
            let subject = match fs::child_directory(&catalog, "agents")
                .and_then(|agents| fs::child_directory(&agents, uri.host()))
                .and_then(|host| fs::child_directory(&host, uri.identity())) {
                Ok(subject) => subject,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io(error)),
            };
            let bytes = match fs::read_regular(&subject, "agent.kdl", MAX_BYTES) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io(error)),
            };
            let source = std::str::from_utf8(&bytes).map_err(|_| refusal("validation-failed", "notes declaration must be UTF-8"))?;
            let parsed = agent_spec::parse_declared_document(&agent_path, source);
            if !parsed.is_valid() { return Err(refusal("validation-failed", "notes declaration is invalid")); }
            let document = parsed.document.expect("valid declaration has a document");
            let [agent] = document.agents.as_slice() else {
                return Err(refusal("validation-failed", "notes source must declare exactly one agent"));
            };
            if agent.identity().and_then(agent_spec::DeclaredValue::as_str) != Some(uri.identity())
                || agent.field("host").and_then(|field| field.argument(0)).and_then(agent_spec::DeclaredValue::as_str)
                    .is_some_and(|host| host != uri.host()) {
                return Err(refusal("forbidden", "notes declaration identifies a different subject"));
            }
            let lifecycle = agent.lifecycle().map_err(|_| refusal("validation-failed", "notes source lifecycle is invalid"))?;
            if lifecycle.desired_state.is_retired() {
                return Err(refusal("not-found", "the notes subject is retired"));
            }
            let mut bindings = agent.fields_named("resource").filter(|binding| {
                binding.property("uri").and_then(agent_spec::DeclaredValue::as_str) == Some(uri.as_str())
            });
            let Some(binding) = bindings.next() else { continue };
            if bindings.next().is_some() || binding.property("inactive-reason").is_some() {
                return Err(refusal("validation-failed", "notes binding is inactive or duplicated"));
            }
            let directory = fs::child_directory(&subject, "resources").map_err(io)?;
            let metadata = directory.metadata().map_err(io)?;
            let generation = digest(format!("{}:{}:{}:{}", uri, digest(&bytes), metadata.dev(), metadata.ino()).as_bytes());
            if let Some(previous) = &resolved {
                if previous.generation != generation {
                    return Err(refusal("private-notes-carrier-conflict", "notes declarations resolve divergent carriers"));
                }
            } else {
                resolved = Some(Resolved { directory, generation });
            }
        }
        resolved.ok_or_else(|| refusal("not-found", "no declared owner-local notes binding exists"))
    }

    pub fn read(&self, node: &str, uri: &str) -> Result<Notes, St3Error> {
        let uri = PrivateNotesUri::parse(uri).map_err(|error| refusal("validation-failed", &error.message))?;
        let resolved = self.resolve(node, &uri)?;
        let snapshot = fs::read_carrier(&resolved.directory, MAX_BYTES).map_err(io)?;
        let fence = NotesFence { carrier_generation: resolved.generation, revision: snapshot.revision };
        let markdown = String::from_utf8(snapshot.bytes.unwrap_or_default()).map_err(|_| refusal("validation-failed", "notes carrier must be UTF-8"))?;
        if !fits_client_response(&markdown) {
            return Err(refusal("validation-failed", "notes exceed the encoded client response limit"));
        }
        Ok(Notes { uri: uri.to_string(), markdown, fence })
    }

    pub(crate) fn attest_pairing(&self, state_dir: &Path, claim: &crate::model::ClaimRecord) -> Result<(), St3Error> {
        let _lock = fs::local_lock(&state_dir.join("private-notes.lock")).map_err(io)?;
        let stamp = pairing_stamp(claim)?;
        let connection = ledger(state_dir)?;
        connection.execute("INSERT INTO notes_pairings(claim_id, stamp) VALUES(?1,?2)
            ON CONFLICT(claim_id) DO NOTHING", params![claim.id, stamp]).map_err(database_error)?;
        let stored: String = connection.query_row("SELECT stamp FROM notes_pairings WHERE claim_id=?1",
            [&claim.id], |row| row.get(0)).map_err(database_error)?;
        if stored != stamp { return Err(refusal("forbidden", "notes pairing authority changed after local attestation")); }
        Ok(())
    }

    pub(crate) fn pairing_attested(&self, state_dir: &Path, claim: &crate::model::ClaimRecord) -> Result<bool, St3Error> {
        let admitted = ledger(state_dir)?.query_row("SELECT stamp FROM notes_pairings WHERE claim_id=?1",
            [&claim.id], |row| row.get::<_, String>(0)).optional().map_err(database_error)?;
        match admitted {
            Some(admitted) => Ok(admitted == pairing_stamp(claim)?),
            None => Ok(false),
        }
    }


    pub fn write(&self, node: &str, state_dir: &Path, actor: &str, key: &str, write: &NotesWrite) -> Result<serde_json::Value, St3Error> {
        if !fits_client_response(&write.markdown) { return Err(refusal("validation-failed", "notes exceed the encoded client response limit")); }
        let uri = PrivateNotesUri::parse(&write.uri).map_err(|error| refusal("validation-failed", &error.message))?;
        // A cross-process owner-local lock coordinates both ledger transitions and all supported writers.
        let _lock = fs::local_lock(&state_dir.join("private-notes.lock")).map_err(io)?;
        let connection = ledger(state_dir)?;
        let mut request = Sha256::new();
        for field in [actor, write.uri.as_str(), write.markdown.as_str()] {
            request.update((field.len() as u64).to_be_bytes());
            request.update(field.as_bytes());
        }
        let request_digest = hex::encode(request.finalize());
        let operation = digest(serde_json::to_string(&(actor, key)).map_err(|_| refusal("internal", "notes key could not be encoded"))?.as_bytes());
        let old = connection.query_row("SELECT request_digest, generation, old_revision, new_revision, result FROM notes_operations WHERE operation=?1", [&operation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?))
        }).optional().map_err(database_error)?;
        if old.as_ref().is_some_and(|old| old.0 != request_digest) {
            return Err(refusal("idempotency-conflict", "the notes idempotency key names a different request"));
        }
        // Revalidate current source/ownership before even returning a private exact-retry receipt.
        let resolved = self.resolve(node, &uri)?;
        let snapshot = fs::read_carrier(&resolved.directory, MAX_BYTES).map_err(io)?;
        let current = snapshot.revision;
        if let Some((_, generation, _, _, result)) = &old {
            if generation != &resolved.generation {
                return Err(refusal("stale-fence", "the notes receipt belongs to a different carrier generation"));
            }
            if let Some(result) = result {
                return serde_json::from_str(result).map_err(|_| refusal("internal", "private notes outcome is invalid"));
            }
        }
        let pending: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM notes_operations WHERE uri=?1 AND result IS NULL AND operation<>?2)",
            params![write.uri, operation], |row| row.get(0)).map_err(database_error)?;
        if pending {
            return Err(refusal("private-notes-indeterminate", "a prior notes replacement requires exact-key recovery before another write"));
        }
        let replace = || {
            fs::replace(&resolved.directory, &operation, write.markdown.as_bytes(), |expected| {
                connection.execute("UPDATE notes_operations SET new_revision=?1 WHERE operation=?2", params![expected, operation])
                    .map_err(|_| std::io::Error::other("private notes recovery ledger could not record prepared replacement"))?;
                Ok(())
            }).map_err(|_| refusal("private-notes-indeterminate", "notes replacement remains pending recovery"))
        };
        if let Some((_, _, before, after, _)) = old {
            if current != before && current != after {
                return Err(refusal("private-notes-indeterminate", "pending notes replacement cannot be reconciled with the current carrier"));
            }
            if current == before {
                replace()?;
            } else {
                // Establish durable directory state before publishing a recovered completion.
                resolved.directory.sync_all().map_err(|_| refusal("private-notes-indeterminate", "notes replacement durability could not be established"))?;
            }
        } else {
            if write.fence.carrier_generation != resolved.generation || write.fence.revision != current {
                return Err(refusal("stale-fence", "the notes carrier generation or revision has changed"));
            }
            connection.execute("INSERT INTO notes_operations(operation, uri, request_digest, generation, old_revision, new_revision) VALUES(?1,?2,?3,?4,?5,'unprepared')",
                params![operation, write.uri, request_digest, resolved.generation, current]).map_err(database_error)?;
            replace()?;
        }
        let successor = fs::read_carrier(&resolved.directory, MAX_BYTES).map_err(io)?;
        let fence = NotesFence { carrier_generation: resolved.generation, revision: successor.revision };
        let result = serde_json::json!({"kind":"action-result", "operation_id":format!("operation/private-notes-{}", &operation[..24]), "status":"completed", "affected_ids":[], "private_notes":fence});
        connection.execute("UPDATE notes_operations SET result=?1 WHERE operation=?2", params![result.to_string(), operation]).map_err(database_error)?;
        Ok(result)
    }
}

fn pairing_stamp(claim: &crate::model::ClaimRecord) -> Result<String, St3Error> {
    // Seal the persisted canonical body: object insertion order is not authority.
    let body = smallclaims::hash::canonical_serialized_json_text(
        &(&claim.id, &claim.origin, &claim.kind, &claim.subject, &claim.body),
    ).map_err(|_| refusal("internal", "notes pairing authority could not be encoded"))?;
    Ok(digest(body.as_bytes()))
}

fn ledger(state_dir: &Path) -> Result<Connection, St3Error> {
    let path = state_dir.join("private-notes.sqlite");
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path).map_err(io)?;
    fs::owner(&file).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() { return Err(refusal("forbidden", "private notes ledger must be a regular owner-local file")); }
    let connection = Connection::open(path).map_err(database_error)?;
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS notes_operations(operation TEXT PRIMARY KEY, uri TEXT NOT NULL, request_digest TEXT NOT NULL, generation TEXT NOT NULL, old_revision TEXT NOT NULL, new_revision TEXT NOT NULL, result TEXT);
        CREATE TABLE IF NOT EXISTS notes_pairings(claim_id TEXT PRIMARY KEY, stamp TEXT NOT NULL);").map_err(database_error)?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_attestation_is_local_immutable_and_cannot_reseal_changed_authority() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let authority = Authority::default();
        let store = crate::store::Store::open_memory("owner").unwrap();
        let claim = store.append_claim(&crate::model::ClaimInput {
            subject: "custom/client/pairing-notes".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: Some("person/operator".into()),
            fields: std::collections::BTreeMap::from([
                ("person_id".into(), serde_json::json!("person/operator")),
                ("credential_hash".into(), serde_json::json!("credential-hash")),
                ("scopes".into(), serde_json::json!(["notes.read"])),
                ("expires_at_unix_ms".into(), serde_json::json!(u64::MAX)),
            ]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        assert!(!authority.pairing_attested(root.path(), &claim).unwrap());
        authority.attest_pairing(root.path(), &claim).unwrap();
        assert!(authority.pairing_attested(root.path(), &claim).unwrap());
        let persisted = store.latest_claim(&claim.subject, Some(&claim.kind)).unwrap().unwrap();
        assert!(authority.pairing_attested(root.path(), &persisted).unwrap(),
            "a committed local pairing must retain authority after canonical graph readback");
        assert!(!authority.pairing_attested(other.path(), &claim).unwrap());
        for field in ["person_id", "credential_hash", "scopes", "expires_at_unix_ms"] {
            let mut changed = claim.clone();
            changed.body["fields"][field] = serde_json::json!("substituted-authority");
            assert!(!authority.pairing_attested(root.path(), &changed).unwrap());
            assert_eq!(authority.attest_pairing(root.path(), &changed).unwrap_err().code, "forbidden");
            assert!(authority.pairing_attested(root.path(), &claim).unwrap());
        }
        let mut foreign = claim;
        foreign.origin = "foreign".into();
        assert!(!authority.pairing_attested(root.path(), &foreign).unwrap());
    }
}

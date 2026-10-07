//! The st daemon's part: say which seat a process is, signed with the node key, so the gateway
//! can tell a seat from its person. The daemon finds the seat from the kernel's word: the
//! process's cgroup scope is the scope the daemon started that seat's terminal in, which the
//! terminal registry records as the `st3.scope-unit` tag beside the seat's `st3.subject`.

use std::path::Path;
use std::sync::OnceLock;

use rusqlite::OptionalExtension as _;

use super::identity::{self, STATEMENT_VERSION, Statement};
use super::protocol::Attestation;
use crate::model::St3Error;
use crate::store::Store;

/// The person this daemon's seats work for: the configured `person`. Set once at startup.
pub static PERSON: OnceLock<String> = OnceLock::new();

/// The seat a scope belongs to, from the terminal registry.
pub fn seat_for_scope(pty_root: &Path, scope: &str) -> Result<Option<String>, St3Error> {
    let observations = st_runtime::PtyRuntime::new(pty_root.to_path_buf())
        .snapshot()
        .map_err(|error| St3Error::new("sekrets-attestation", format!("{error:#}")))?;
    Ok(observations
        .into_iter()
        .filter(|observation| observation.status == "running")
        .find(|observation| {
            observation.tags.get("st3.scope-unit").map(String::as_str) == Some(scope)
        })
        .and_then(|observation| observation.tags.get("st3.subject").cloned()))
}

/// This node and the public key it signs attestations with.
pub fn node_key(store: &Store, node: &str) -> Result<(String, String), St3Error> {
    let key = store.keyring.node().ok_or_else(|| {
        St3Error::new(
            "sekrets-no-node-key",
            "this daemon has no node key yet; it makes one when it creates or joins a fleet",
        )
    })?;
    Ok((format!("host/{node}"), key.public().to_owned()))
}

/// Sign which seat process `pid` is, for the gateway nonce `nonce`. `ancestor` is the agent the
/// process's ancestry names, when it names one; it must agree with the scope.
pub fn attest(
    store: &Store,
    node: &str,
    pty_root: &Path,
    pid: u32,
    ancestor: Option<&str>,
    nonce: &str,
) -> Result<Attestation, St3Error> {
    let refuse = |message: String| St3Error::new("sekrets-attestation-refused", message);
    let person = PERSON.get().ok_or_else(|| {
        refuse("set `person` in this daemon's configuration to use sekrets from seats".into())
    })?;
    if nonce.is_empty() || nonce.len() > 128 {
        return Err(refuse("the gateway nonce is missing".into()));
    }
    let pid_i32 = i32::try_from(pid).map_err(|_| refuse(format!("pid {pid} is out of range")))?;
    let cgroup = identity::process_cgroup(pid_i32)
        .ok_or_else(|| refuse(format!("cannot read the cgroup of process {pid}")))?;
    let pid_start = identity::process_start(pid_i32)
        .ok_or_else(|| refuse(format!("cannot read the start time of process {pid}")))?;
    let scope = cgroup.rsplit('/').next().unwrap_or_default();
    let agent = seat_for_scope(pty_root, scope)?.ok_or_else(|| {
        refuse(format!(
            "process {pid} runs in {cgroup}, which is no running seat's scope"
        ))
    })?;
    if !agent.starts_with("agent/") {
        return Err(refuse(format!("{scope} belongs to {agent}, not an agent")));
    }
    if let Some(ancestor) = ancestor
        && ancestor != agent
    {
        return Err(refuse(format!(
            "process {pid} runs in {agent}'s scope but descends from {ancestor}"
        )));
    }
    let (node_subject, _) = node_key(store, node)?;
    let key = store.keyring.node().expect("node_key checked it");
    let revision = store.desired_revision(&agent).ok().flatten();
    let statement = Statement {
        version: STATEMENT_VERSION,
        node: node_subject,
        person: person.clone(),
        agent,
        revision,
        cgroup,
        pid: pid_i32,
        pid_start,
        nonce: nonce.to_owned(),
        issued_at_unix_ms: super::store::now_unix_ms(),
    };
    let text = serde_json::to_string(&statement)
        .map_err(|error| St3Error::new("internal", error.to_string()))?;
    let signature = key.sign(&identity::signing_message(&text));
    Ok(Attestation {
        statement: text,
        signature,
    })
}

impl Store {
    /// The revision of a subject's current declaration.
    pub fn desired_revision(&self, subject: &str) -> anyhow::Result<Option<String>> {
        let connection = self.readers.get();
        Ok(connection
            .query_row(
                "SELECT revision FROM desired WHERE subject=?1",
                [subject],
                |row| row.get(0),
            )
            .optional()?)
    }
}

/// The claim one gateway log entry becomes, or `None` for an entry this daemon does not record:
/// another person's call, which their own daemon records.
pub fn claim_for(
    node: &str,
    person: &str,
    entry: &super::store::LogEntry,
) -> Option<smallclaims::ClaimInput> {
    use serde_json::{Value, json};
    if entry.caller_person.as_deref() != Some(person) {
        return None;
    }
    let subject = match &entry.profile {
        Some(profile) => format!("sekret/{node}/{profile}"),
        None => format!("sekret/{node}"),
    };
    let detail = &entry.detail;
    let mut fields = std::collections::BTreeMap::<String, Value>::new();
    fields.insert("seq".into(), json!(entry.seq));
    fields.insert("person".into(), json!(person));
    fields.insert("at_unix_ms".into(), json!(entry.at_unix_ms));
    if let Some(actor) = &entry.actor {
        fields.insert("caller".into(), json!(actor));
    }
    if let Some(profile) = &entry.profile {
        fields.insert("profile".into(), json!(profile));
    }
    let copy = |fields: &mut std::collections::BTreeMap<String, Value>, from: &str, to: &str| {
        if let Some(value) = detail.get(from).filter(|value| !value.is_null()) {
            fields.insert(to.into(), value.clone());
        }
    };
    let kind = match entry.event.as_str() {
        "call" => {
            entry.actor.as_ref()?;
            for name in ["argv", "cwd", "grant", "login", "tty"] {
                copy(&mut fields, name, name);
            }
            "sekret.called"
        }
        "exited" => {
            copy(&mut fields, "call", "call");
            copy(&mut fields, "code", "exit_code");
            copy(&mut fields, "signal", "signal");
            copy(&mut fields, "error", "error");
            "sekret.exited"
        }
        "refused" => {
            copy(&mut fields, "argv", "argv");
            copy(&mut fields, "reason", "reason");
            "sekret.refused"
        }
        change if st3_schema::SEKRET_CHANGES.contains(&change) => {
            fields.insert("change".into(), json!(change));
            fields.insert("detail".into(), detail.clone());
            "sekret.changed"
        }
        _ => return None,
    };
    Some(smallclaims::ClaimInput {
        subject,
        kind: kind.into(),
        actor: None,
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("sekret:{node}:{}", entry.seq)),
    })
}

/// Record this host's gateway log as claims, from the last entry recorded on. Opt-in by
/// existence: until a gateway listens at `socket`, this checks once a minute and does nothing.
pub fn spawn_importer(
    store: std::sync::Arc<Store>,
    node: String,
    state_dir: std::path::PathBuf,
    socket: std::path::PathBuf,
) {
    let Some(person) = PERSON.get().cloned() else {
        return;
    };
    let cursor_path = state_dir.join("sekrets-import.cursor");
    std::thread::Builder::new()
        .name("sekrets-import".into())
        .spawn(move || {
            let mut cursor = std::fs::read_to_string(&cursor_path)
                .ok()
                .and_then(|text| text.trim().parse::<i64>().ok())
                .unwrap_or(0);
            let mut reported = false;
            loop {
                if !socket.exists() {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    continue;
                }
                match import_until_error(&store, &node, &person, &socket, &cursor_path, &mut cursor)
                {
                    Ok(()) => {}
                    Err(error) => {
                        if !reported {
                            eprintln!("st3: sekrets: cannot record the gateway's log: {error:#}");
                            reported = true;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(30));
                    }
                }
            }
        })
        .expect("start the sekrets importer thread");
}

fn import_until_error(
    store: &Store,
    node: &str,
    person: &str,
    socket: &Path,
    cursor_path: &Path,
    cursor: &mut i64,
) -> anyhow::Result<()> {
    let connection = super::client::Connection::open(socket)?;
    loop {
        let value = connection.manage(&super::protocol::Request::Log {
            after: *cursor,
            limit: 200,
            wait_ms: 55_000,
        })?;
        let entries: Vec<super::store::LogEntry> = serde_json::from_value(value)?;
        let Some(_) = entries.last() else {
            continue;
        };
        import_entries(&entries, node, person, cursor_path, cursor, |claim| {
            store.append_claim(claim).map(|_| ())
        })?;
    }
}

// Advance a gateway page only after all retryable storage failures are excluded. Entries
// before a failed append can be replayed safely using their existing idempotency keys.
fn import_entries(
    entries: &[super::store::LogEntry],
    node: &str,
    person: &str,
    cursor_path: &Path,
    cursor: &mut i64,
    mut append: impl FnMut(&smallclaims::ClaimInput) -> Result<(), St3Error>,
) -> anyhow::Result<()> {
    let Some(last) = entries.last().map(|entry| entry.seq) else { return Ok(()) };
    for entry in entries {
        if let Some(claim) = claim_for(node, person, entry) {
            match append(&claim) {
                Ok(()) => {}
                Err(error) if error.code == "internal" || error.is_sqlite_contention() => {
                    return Err(error.into());
                }
                Err(error) => eprintln!("st3: sekrets: skipped gateway log entry {}: {error:#}", entry.seq),
            }
        }
    }
    // Write the durable cursor first; a failed file write must not leave the in-memory
    // cursor past the entry either.
    std::fs::write(cursor_path, format!("{last}\n"))?;
    *cursor = last;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sekrets::store::LogEntry;
    use serde_json::json;

    fn entry(event: &str, caller_person: &str, detail: serde_json::Value) -> LogEntry {
        LogEntry {
            seq: 7,
            at_unix_ms: 1_000,
            caller_person: Some(caller_person.into()),
            owner_person: Some("person/ada".into()),
            event: event.into(),
            actor: Some("agent/fleet/fixture-web/builder".into()),
            profile: Some("ada/agent-gh".into()),
            detail,
        }
    }

    #[test]
    fn busy_gateway_append_keeps_cursor_and_retries_the_same_entry() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor");
        std::fs::write(&path, "6\n").unwrap();
        let entries = vec![entry("call", "person/ada", json!({"argv":["fixture"]}))];
        let mut cursor = 6;
        let mut attempts = Vec::new();
        let first = import_entries(&entries, "example", "person/ada", &path, &mut cursor, |claim| {
            attempts.push(claim.idempotency_key.clone());
            Err(smallclaims::error::internal(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code), None)))
        });
        assert!(first.is_err());
        assert_eq!(cursor, 6);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "6\n");
        import_entries(&entries, "example", "person/ada", &path, &mut cursor, |claim| {
            attempts.push(claim.idempotency_key.clone());
            Ok(())
        }).unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
        assert_eq!(cursor, 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "7\n");
        }
    }
    #[test]
    fn graph_refusal_remains_skippable_but_file_error_does_not_advance_memory_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor");
        let entries = vec![entry("call", "person/ada", json!({"argv":["fixture"]}))];
        let mut cursor = 6;
        import_entries(&entries, "example", "person/ada", &path, &mut cursor,
            |_| Err(St3Error::new("rule-denied", "fixture refusal"))).unwrap();
        assert_eq!(cursor, 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "7\n");
        cursor = 6;
        assert!(import_entries(&entries, "example", "person/ada", dir.path(), &mut cursor,
            |_| Ok(())).is_err());
        assert_eq!(cursor, 6);
    }
    #[test]
    fn each_entry_becomes_one_claim_its_callers_daemon_records() {
        let call = claim_for(
            "example",
            "person/ada",
            &entry(
                "call",
                "person/ada",
                json!({"argv": ["gh", "pr", "list"], "cwd": "/home/example/web", "grant": null, "login": false, "tty": false}),
            ),
        )
        .unwrap();
        assert_eq!(call.kind, "sekret.called");
        assert_eq!(call.subject, "sekret/example/ada/agent-gh");
        assert_eq!(call.fields["argv"], json!(["gh", "pr", "list"]));
        assert!(!call.fields.contains_key("grant"));
        assert_eq!(call.idempotency_key.as_deref(), Some("sekret:example:7"));
        let exited = claim_for(
            "example",
            "person/ada",
            &entry(
                "exited",
                "person/ada",
                json!({"call": 6, "code": 0, "signal": null}),
            ),
        )
        .unwrap();
        assert_eq!(exited.kind, "sekret.exited");
        assert_eq!(exited.fields["exit_code"], 0);
        let changed = claim_for(
            "example",
            "person/ada",
            &entry("put", "person/ada", json!({"name": "GH_TOKEN"})),
        )
        .unwrap();
        assert_eq!(changed.kind, "sekret.changed");
        assert_eq!(changed.fields["change"], "put");
        // Robin's daemon records Robin's calls with Ada's profile; Ada's does not.
        assert!(
            claim_for(
                "example",
                "person/ada",
                &entry("call", "person/robin", json!({}))
            )
            .is_none()
        );
        for claim in [call, exited, changed] {
            st3_schema::registry()
                .validate_claim(&claim.subject, &claim.kind, &claim.fields)
                .unwrap_or_else(|error| panic!("{}: {error:?}", claim.kind));
        }
    }
}

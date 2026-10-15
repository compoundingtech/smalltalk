//! Owner-local launch admission and revocation. No replicated schema or receipt vocabulary.
use super::*;
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let has_subject_epochs: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='local_runtime_start_observation_subjects')",
        [], |row| row.get(0),
    )?;
    if !has_subject_epochs {
        // The installation transaction replaces earlier draft triggers once. No
        // ledger backfill is needed: existing results predate every new capture.
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-h6ca965e20fa2 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "DROP TRIGGER IF EXISTS local_runtime_starts_observation_insert;
             DROP TRIGGER IF EXISTS local_runtime_starts_observation_update;
             DROP TRIGGER IF EXISTS local_runtime_starts_observation_delete;",
        )?;
    }
    transaction.execute_batch(/* sqlite-large-value: allow-for-now lv-h0f3b14e10aa0 owner=st3::store::runtime_starts TODO=chunk-or-reference-variable-columns */ r#"
CREATE TABLE IF NOT EXISTS local_runtime_start_observation_generation (
    id INTEGER PRIMARY KEY CHECK(id=1),
    value INTEGER NOT NULL CHECK(typeof(value)='integer' AND value>=0)
);
INSERT OR IGNORE INTO local_runtime_start_observation_generation VALUES(1,0);
CREATE TABLE IF NOT EXISTS local_runtime_start_observation_subjects (
    subject TEXT PRIMARY KEY,
    value INTEGER NOT NULL CHECK(typeof(value)='integer' AND value>=0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_runtime_starts (
    id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    owner TEXT NOT NULL,
    body TEXT NOT NULL,
    revoked_by TEXT,
    finished INTEGER NOT NULL DEFAULT 0,
    result TEXT,
    applied INTEGER NOT NULL DEFAULT 0,
    cleanup_verified INTEGER NOT NULL DEFAULT 0,
    dispatched INTEGER NOT NULL DEFAULT 0,
    receipted INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS local_runtime_starts_observation_insert AFTER INSERT ON local_runtime_starts
BEGIN
    UPDATE local_runtime_start_observation_generation SET value=value+1 WHERE id=1;
    INSERT INTO local_runtime_start_observation_subjects(subject,value)
    VALUES(NEW.subject,(SELECT value FROM local_runtime_start_observation_generation WHERE id=1))
    ON CONFLICT(subject) DO UPDATE SET value=excluded.value;
END;
CREATE TRIGGER IF NOT EXISTS local_runtime_starts_observation_update AFTER UPDATE ON local_runtime_starts
BEGIN
    UPDATE local_runtime_start_observation_generation SET value=value+1 WHERE id=1;
    INSERT INTO local_runtime_start_observation_subjects(subject,value)
    VALUES(NEW.subject,(SELECT value FROM local_runtime_start_observation_generation WHERE id=1))
    ON CONFLICT(subject) DO UPDATE SET value=excluded.value;
    INSERT INTO local_runtime_start_observation_subjects(subject,value)
    SELECT OLD.subject,value FROM local_runtime_start_observation_generation
    WHERE id=1 AND OLD.subject IS NOT NEW.subject
    ON CONFLICT(subject) DO UPDATE SET value=excluded.value;
END;
CREATE TRIGGER IF NOT EXISTS local_runtime_starts_observation_delete AFTER DELETE ON local_runtime_starts
BEGIN
    UPDATE local_runtime_start_observation_generation SET value=value+1 WHERE id=1;
    INSERT INTO local_runtime_start_observation_subjects(subject,value)
    VALUES(OLD.subject,(SELECT value FROM local_runtime_start_observation_generation WHERE id=1))
    ON CONFLICT(subject) DO UPDATE SET value=excluded.value;
END;
CREATE INDEX IF NOT EXISTS local_runtime_starts_subject ON local_runtime_starts(subject,id DESC);
CREATE INDEX IF NOT EXISTS local_runtime_starts_subject_request
ON local_runtime_starts(subject,CAST(json_extract(body,'$.start_request_position') AS INTEGER) DESC);
CREATE INDEX IF NOT EXISTS local_runtime_starts_verified_subject_request
ON local_runtime_starts(subject,CAST(json_extract(body,'$.start_request_position') AS INTEGER) DESC)
WHERE json_extract(result,'$.verification') IN ('operation-incarnation','operation-ended','operation-retired')
 AND json_type(result,'$.incarnation')='text';
CREATE INDEX IF NOT EXISTS local_runtime_starts_pending ON local_runtime_starts(id)
WHERE finished=0 OR applied=0 OR (revoked_by IS NOT NULL AND cleanup_verified=0)
 OR (json_extract(result,'$.error') IS NOT NULL AND cleanup_verified=0);
CREATE UNIQUE INDEX IF NOT EXISTS local_runtime_starts_active_subject ON local_runtime_starts(subject)
WHERE finished=0 OR applied=0 OR (revoked_by IS NOT NULL AND cleanup_verified=0)
 OR (json_extract(result,'$.error') IS NOT NULL AND cleanup_verified=0);
CREATE TABLE IF NOT EXISTS local_runtime_start_evidence (
    operation_id TEXT NOT NULL,
    observation_id INTEGER NOT NULL,
    PRIMARY KEY(operation_id, observation_id)
);
CREATE INDEX IF NOT EXISTS local_runtime_start_evidence_observation ON local_runtime_start_evidence(observation_id);
"#)?;
    // Capture the format boundary before admission can write any operation-bound
    // request. Never infer a later boundary from an already-used ledger.
    let has_cuts: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='publication-operation-format-shared-cut')
         AND EXISTS(SELECT 1 FROM meta WHERE key='publication-operation-format-local-cut')",
        [],
        |row| row.get(0),
    )?;
    if !has_cuts {
        let has_operations: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_runtime_starts)",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !has_operations,
            "runtime-start ledger has operations but lacks immutable publication format cuts; preserve this store and roll forward with its original format-boundary evidence"
        );
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-haaa22927d7ba owner=st3::store::runtime_starts TODO=prepare-body-and-predecessor-values */ "INSERT OR IGNORE INTO meta(key,value)
             SELECT 'publication-operation-format-shared-cut', CAST(COALESCE(MAX(store_index),0) AS TEXT) FROM claims;
             INSERT OR IGNORE INTO meta(key,value)
             SELECT 'publication-operation-format-local-cut', CAST(COALESCE(MAX(id),0) AS TEXT) FROM local_observations;",
        )?;
    }
    transaction.commit()?;
    Ok(())
}

thread_local! {
    static RUNTIME_OBSERVATION_CAPTURE: std::cell::RefCell<Option<(std::path::PathBuf,u64)>> = const { std::cell::RefCell::new(None) };
}

/// Physical captures are synchronous and thread-scoped. Never carry the witness
/// to another thread or allow a nested capture to erase its predecessor.
pub(crate) struct RuntimeObservationCapture {
    previous: Option<(std::path::PathBuf, u64)>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Drop for RuntimeObservationCapture {
    fn drop(&mut self) {
        RUNTIME_OBSERVATION_CAPTURE.with(|capture| {
            capture.replace(self.previous.take());
        });
    }
}

const PENDING: &str = "finished=0 OR applied=0 OR (revoked_by IS NOT NULL AND cleanup_verified=0) OR (json_extract(result,'$.error') IS NOT NULL AND cleanup_verified=0)";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeStartOperation {
    pub id: String,
    pub subject: String,
    pub owner: String,
    pub body: Value,
    pub revoked_by: Option<String>,
    pub finished: bool,
    pub result: Option<Value>,
    pub applied: bool,
    pub cleanup_verified: bool,
    pub dispatched: bool,
    pub receipted: bool,
}

/// Nonblocking process-death-safe admission. Controls never acquire this lock.
pub struct RuntimeStartLock(File);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeStartAdmission {
    Admitted,
    SeatBusy,
    CapacityBusy,
    Superseded,
}

fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuntimeStartOperation> {
    let decode = |column| -> rusqlite::Result<Value> {
        let text: String = row.get(column)?;
        serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let result: Option<String> = row.get(6)?;
    Ok(RuntimeStartOperation {
        id: row.get(0)?,
        subject: row.get(1)?,
        owner: row.get(2)?,
        body: decode(3)?,
        revoked_by: row.get(4)?,
        finished: row.get(5)?,
        result: result
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        applied: row.get(7)?,
        cleanup_verified: row.get(8)?,
        dispatched: row.get(9)?,
        receipted: row.get(10)?,
    })
}

fn pin_evidence(tx: &Transaction<'_>, operation: &str, origin: &str, evidence: &str) -> Result<()> {
    if let Some(position) =
        evidence.strip_prefix(&format!("{LOCAL_OBSERVATION_ID_PREFIX}{origin}/"))
    {
        let position: i64 = position.parse()?;
        tx.execute(/* sqlite-large-value: allow-for-now lv-h6ab425e3739e owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO local_runtime_start_evidence(operation_id,observation_id) VALUES (?1,?2)",
            params![operation, position])?;
    }
    Ok(())
}

fn release_evidence(tx: &Transaction<'_>, operation: &str) -> rusqlite::Result<()> {
    tx.execute(
        /* sqlite-large-value: allow-for-now lv-h72e5255fabef owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM local_runtime_start_evidence WHERE operation_id=?1 AND EXISTS (
        SELECT 1 FROM local_runtime_starts WHERE id=?1 AND finished=1 AND applied=1
          AND (revoked_by IS NULL OR cleanup_verified=1)
          AND (json_extract(result,'$.error') IS NULL OR cleanup_verified=1))",
        [operation],
    )?;
    Ok(())
}

fn retained_runtime_start_request(
    connection: &Connection,
    origin: &str,
    id: &str,
) -> Result<Option<ClaimRecord>> {
    if id.starts_with(LOCAL_OBSERVATION_ID_PREFIX) {
        let Some(position) = id.strip_prefix(&format!("{LOCAL_OBSERVATION_ID_PREFIX}{origin}/"))
        else {
            return Ok(None);
        };
        let Ok(position) = position.parse::<i64>() else {
            return Ok(None);
        };
        connection.query_row(
            "SELECT id,after_store_index,subject,kind,actor,body,observed_at_unix_ms FROM local_observations WHERE id=?1",
            [position], |row| local_observation_from_row(origin, row),
        ).optional().map_err(Into::into)
    } else {
        connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-hea4f182aac9d owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!("SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims WHERE id=?1 AND {UNREPAIRED_CLAIM}"))?
            .query_row(/* sqlite-large-value: allow-for-now lv-hdf719d141b14 owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ [id], claim_from_row).optional().map_err(Into::into)
    }
}

fn same_runtime_start_request(actual: &ClaimRecord, expected: &ClaimRecord) -> bool {
    actual.id == expected.id
        && actual.subject == expected.subject
        && actual.kind == expected.kind
        && actual.origin == expected.origin
        && actual.actor == expected.actor
        && actual.body == expected.body
}

fn runtime_start_requests_retained(
    connection: &Connection,
    origin: &str,
    body: &Value,
) -> Result<bool> {
    for (id, snapshot) in [
        ("start_request", "start_request_receipt"),
        ("control_request", "control_request_receipt"),
    ] {
        if id == "control_request" && body[id].is_null() && body[snapshot].is_null() {
            continue;
        }
        if body[snapshot].is_null() {
            return Ok(false);
        }
        let expected: ClaimRecord = serde_json::from_value(body[snapshot].clone())?;
        if body[id].as_str() != Some(expected.id.as_str())
            || !retained_runtime_start_request(connection, origin, &expected.id)?
                .is_some_and(|actual| same_runtime_start_request(&actual, &expected))
        {
            return Ok(false);
        }
    }
    if !body["request"].is_null() {
        let expected: ClaimRecord = serde_json::from_value(body["request"].clone())?;
        if !retained_runtime_start_request(connection, origin, &expected.id)?
            .is_some_and(|actual| same_runtime_start_request(&actual, &expected))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bind_runtime_start_result(operation: &RuntimeStartOperation, origin: &str, result: &mut Value) {
    result["operation_id"] = Value::from(operation.id.clone());
    result["subject"] = Value::from(operation.subject.clone());
    result["owner"] = Value::from(operation.owner.clone());
    result["origin"] = Value::from(origin);
    result["runtime_id"] = operation.body["member"]["runtime_id"].clone();
    result["terminal"] = operation.body["member"]["terminal"].clone();
    result["runtime_kind"] = Value::from(if result["terminal"] == true {
        "pty"
    } else {
        "exec"
    });
    for key in [
        "selected_token",
        "desired_token",
        "expected_incarnation",
        "start_request",
        "control_request",
    ] {
        result[key] = operation.body[key].clone();
    }
}

fn retain_runtime_start_closure(
    operation: &RuntimeStartOperation,
    origin: &str,
    result: &mut Value,
    witness: Value,
) -> Result<()> {
    bind_runtime_start_result(operation, origin, result);
    let proof = json!({"version":1,"operation_id":operation.id,"subject":operation.subject,
        "owner":operation.owner,"origin":origin,"runtime_id":result["runtime_id"],
        "terminal":result["terminal"],"runtime_kind":result["runtime_kind"],
        "selected_token":result["selected_token"],"desired_token":result["desired_token"],
        "expected_incarnation":result["expected_incarnation"],"incarnation":result["incarnation"],
        "verification":result["verification"],"start_request":result["start_request"],
        "start_request_receipt":operation.body["start_request_receipt"],"start_request_position":operation.body["start_request_position"],
        "control_request":result["control_request"],"control_request_receipt":operation.body["control_request_receipt"],
        "requester_restart":operation.body["request"],"witness":witness});
    result["closure_proof_id"] = Value::from(format!("start-proof/{}", canonical_hash(&proof)?));
    result["closure_proof"] = proof;
    Ok(())
}

fn closed_runtime_start_result(
    operation: &RuntimeStartOperation,
    origin: &str,
    result: &mut Value,
) -> Result<()> {
    bind_runtime_start_result(operation, origin, result);
    if result["error"].is_string() && result["producer_closed"] == true {
        result["verification"] = Value::from("producer-closed");
        result["phase"] = Value::from("failed");
        let kind = result["closure_kind"]
            .as_str()
            .unwrap_or("positive-producer-proof")
            .to_owned();
        retain_runtime_start_closure(operation, origin, result, json!({"kind":kind}))?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn capture_runtime_observations(&self) -> Result<RuntimeObservationCapture> {
        let epoch = self.readers.get().query_row(
            "SELECT value FROM local_runtime_start_observation_generation WHERE id=1",
            [],
            |row| row.get::<_, u64>(0),
        )?;
        let previous = RUNTIME_OBSERVATION_CAPTURE
            .with(|capture| capture.replace(Some((self.path.clone(), epoch))));
        Ok(RuntimeObservationCapture {
            previous,
            _thread: std::marker::PhantomData,
        })
    }

    /// A late physical snapshot cannot turn the prior target into a replacement
    /// for a newly verified launch. Admission order comes from the exact local
    /// request position, never UUID creation time or claim publication time.
    pub(crate) fn append_reconciled_runtime_observation(
        &self,
        input: &ClaimInput,
    ) -> Result<Option<ClaimRecord>> {
        anyhow::ensure!(
            input.kind == "runtime.observed" && input.actor.is_none(),
            "expected owner runtime observation"
        );
        let capture_epoch = RUNTIME_OBSERVATION_CAPTURE.with(|capture| {
            capture
                .borrow()
                .as_ref()
                .filter(|(path, _)| path == &self.path)
                .map(|(_, epoch)| *epoch)
        });
        let admission = |connection: &Connection| -> Result<(), St3Error> {
            let latest: Option<RuntimeStartOperation> = connection.query_row(
                "SELECT * FROM local_runtime_starts WHERE subject=?1
                 AND json_extract(result,'$.verification') IN ('operation-incarnation','operation-ended','operation-retired')
                 AND json_type(result,'$.incarnation')='text'
                 ORDER BY CAST(json_extract(body,'$.start_request_position') AS INTEGER) DESC LIMIT 1",
                [&input.subject], read).optional().map_err(internal)?;
            if let Some(operation) = latest
                && let Some(result) = &operation.result
                && let Some(actual) = result["incarnation"].as_str()
            {
                let observed = input.fields.get("incarnation_id").and_then(Value::as_str);
                let prior = operation.body["expected_incarnation"].as_str();
                let current = connection
                    .query_row(
                        "SELECT COALESCE((SELECT value FROM local_runtime_start_observation_subjects WHERE subject=?1),0)",
                        [&input.subject],
                        |row| row.get::<_, u64>(0),
                    )
                    .map_err(internal)?;
                // The capture serial predates physical reads. Only this subject's
                // later mutation invalidates it; other subjects cannot starve exits.
                let stale_capture = capture_epoch.is_none_or(|captured| current > captured);
                if observed != Some(actual)
                    && (stale_capture || (observed.is_some() && observed == prior))
                {
                    return Err(St3Error::new(
                        "stale-runtime-observation",
                        "physical snapshot precedes the verified start incarnation",
                    ));
                }
            }
            Ok(())
        };
        match append_claim_with_commit_context(
            &self.graph,
            input,
            None,
            None,
            None,
            None,
            ClaimCommitContext {
                admission: Some(&admission),
                ..Default::default()
            },
        ) {
            Ok((record, _)) => Ok(Some(record)),
            Err(error) if error.code == "stale-runtime-observation" => {
                crate::reconcile_telemetry::record_runtime_observation_drop();
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Revocation, declaration selection and all launch receipts linearize in one short
    /// writer transaction. A stale completion cannot leave a success/starting receipt.
    pub(crate) fn apply_runtime_start_result(
        &self,
        id: &str,
        inputs: &[ClaimInput],
    ) -> Result<bool> {
        for input in inputs {
            anyhow::ensure!(
                local_retention(&input.kind)
                    || (input.actor.is_none()
                        && (system_local_retention(&input.kind)
                            || input.kind == "runtime.observed")),
                "launch completion uses local action receipts and the existing runtime observation"
            );
            validate_local_observation(input)?;
        }
        self.connection
            .batched(|tx| -> Result<bool> {
                let operation =
                    tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
                if operation.revoked_by.is_some() || operation.applied || !operation.finished {
                    return Ok(false);
                }
                if operation.result.as_ref().is_some_and(|result| {
                    !result["error"].is_string()
                        && result["verification"] != "operation-incarnation"
                }) {
                    return Ok(false);
                }
                if !runtime_start_requests_retained(tx, self.origin(), &operation.body)? {
                    if operation.result.as_ref().is_some_and(|result| {
                        result["producer_closed"] == true
                            && result["verification"] == "producer-closed"
                            && result["closure_proof_id"].is_string()
                    }) {
                        // Positive no-effect proof survives repair/retraction. Do not
                        // manufacture receipts citing evidence that no longer exists.
                        tx.execute(
                            /* sqlite-large-value: allow-for-now lv-hff4778b9ea35 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET receipted=1 WHERE id=?1",
                            [id],
                        )?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                let selected =
                    current_desired_row_tx(tx, &operation.subject)?.map(|row| row.claim_id);
                let expected = operation.body["selected_token"].as_str().map(str::to_owned);
                if selected != expected {
                    return Ok(false);
                }
                let result = operation
                    .result
                    .as_ref()
                    .context("finished start has no result")?;
                for input in inputs {
                    if input.subject != operation.subject {
                        return Ok(false);
                    }
                    if input.kind == "runtime.observed"
                        && result["verification"] == "operation-incarnation"
                        && (input.fields.get("incarnation_id") != Some(&result["incarnation"])
                            || input.fields.get("runtime_id")
                                != Some(&operation.body["member"]["runtime_id"])
                            || input.fields.get("terminal")
                                != Some(&operation.body["member"]["terminal"]))
                    {
                        return Ok(false);
                    }
                    if input
                        .fields
                        .get("action")
                        .is_some_and(|action| action == "start")
                    {
                        if input
                            .fields
                            .get("operation")
                            .is_some_and(|bound| bound != id)
                            || input.fields.get("runtime_id").is_some_and(|bound| {
                                bound != &operation.body["member"]["runtime_id"]
                            })
                            || input
                                .fields
                                .get("terminal")
                                .is_some_and(|bound| bound != &operation.body["member"]["terminal"])
                            || input
                                .fields
                                .get("desired_token")
                                .is_some_and(|bound| bound != &operation.body["desired_token"])
                        {
                            return Ok(false);
                        }
                        if input.kind == "runtime.action.succeeded"
                            && (result["error"].is_string()
                                || result["verification"] != "operation-incarnation"
                                || !result["incarnation"].is_string()
                                || !operation.body["member"]["runtime_id"].is_string()
                                || !operation.body["member"]["terminal"].is_boolean()
                                || !operation.body["desired_token"].is_string()
                                || input
                                    .fields
                                    .get("incarnation_id")
                                    .is_some_and(|bound| bound != &result["incarnation"]))
                        {
                            return Ok(false);
                        }
                        if input.kind == "runtime.action.failed" && !result["error"].is_string() {
                            return Ok(false);
                        }
                    }
                }
                if operation.receipted {
                    return Ok(true);
                }
                let mut observation_receipt = None;
                let receipts = inputs
                    .iter()
                    .map(|input| -> Result<String> {
                        let mut input = input.clone();
                        if input
                            .fields
                            .get("action")
                            .is_some_and(|action| action == "start")
                        {
                            input.fields.insert("operation".into(), Value::from(id));
                            if let Some(runtime) = operation.body["member"]["runtime_id"].as_str() {
                                input
                                    .fields
                                    .insert("runtime_id".into(), Value::from(runtime));
                            }
                            if let Some(terminal) = operation.body["member"]["terminal"].as_bool() {
                                input
                                    .fields
                                    .insert("terminal".into(), Value::from(terminal));
                            }
                            if let Some(token) = operation.body["desired_token"].as_str() {
                                input
                                    .fields
                                    .insert("desired_token".into(), Value::from(token));
                            }
                            input.fields.remove("operation_status");
                            if input.kind == "runtime.action.succeeded" {
                                input
                                    .fields
                                    .insert("operation_status".into(), Value::from("verified"));
                                input
                                    .fields
                                    .insert("incarnation_id".into(), result["incarnation"].clone());
                            } else if input.kind == "runtime.action.failed"
                                && result["verification"] == "producer-closed"
                                && result["producer_closed"] == true
                            {
                                input.fields.insert(
                                    "operation_status".into(),
                                    Value::from("producer-closed"),
                                );
                            }
                            let request = operation.body["start_request"]
                                .as_str()
                                .context("start result has no original request")?;
                            if !input.evidence.iter().any(|evidence| evidence == request) {
                                input.evidence.push(request.into());
                            }
                            if let Some(control) = operation.body["control_request"].as_str()
                                && !input.evidence.iter().any(|evidence| evidence == control)
                            {
                                input.evidence.push(control.into());
                            }
                            validate_local_observation(&input)?;
                        }
                        let claim = if input.kind == "runtime.observed" {
                            // Reconciliation can observe a published child while its
                            // launcher is still returning. A delayed completion must
                            // not regress that exact runtime from running to starting.
                            let current: Option<String> = tx
                                .query_row(
                                    /* sqlite-large-value: allow-for-now lv-h00efaf58440d owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!(
                                        "{} LIMIT 1",
                                        newest_claims_of_kind_query(
                                            "claims.id",
                                            "runtime.observed"
                                        )
                                    ),
                                    params![input.subject, i64::MAX],
                                    |row| row.get(0),
                                )
                                .optional()?;
                            let current = current
                                .map(|id| claim_by_id_tx(tx, &id))
                                .transpose()?
                                .flatten();
                            if input
                                .fields
                                .get("status")
                                .is_some_and(|status| status == "starting")
                                && let Some(current) = current.filter(|current| {
                                    current.origin == self.origin()
                                        && current.body.pointer("/fields/incarnation_id")
                                            == Some(&result["incarnation"])
                                        && current.body.pointer("/fields/runtime_id")
                                            == Some(&operation.body["member"]["runtime_id"])
                                        && current
                                            .body
                                            .pointer("/fields/status")
                                            .is_some_and(|status| status != "starting")
                                })
                            {
                                let key = canonical::claim_key(tx, &current.id)?;
                                observation_receipt = Some(json!({"receipt":current,
                                    "canonical_sortable_key":canonical::sortable_key(&key)}));
                                return Ok(current.id);
                            }
                            let mut predecessors = latest_claim_id_tx(tx, &input.subject)?
                                .into_iter()
                                .collect::<Vec<_>>();
                            for evidence in &input.evidence {
                                if claim_by_id_tx(tx, evidence)?
                                    .is_some_and(|claim| claim.subject == input.subject)
                                {
                                    predecessors.push(evidence.clone());
                                }
                            }
                            predecessors.sort();
                            predecessors.dedup();
                            append_receipt_claim_tx(
                                tx,
                                self.origin(),
                                &input.subject,
                                &input.kind,
                                input.actor.as_deref(),
                                &json!({"fields": input.fields, "evidence": input.evidence}),
                                &predecessors,
                                None,
                                input
                                    .idempotency_key
                                    .as_deref()
                                    .context("runtime start observation needs its operation key")?,
                            )?
                        } else {
                            insert_local_observation_tx(tx, self.origin(), &input, now_ms())?.0
                        };
                        if claim.kind == "runtime.observed"
                            && result["verification"] == "operation-incarnation"
                        {
                            anyhow::ensure!(
                                claim.body.pointer("/fields/incarnation_id")
                                    == Some(&result["incarnation"]),
                                "start observation must bind the verified actual incarnation"
                            );
                            let key = canonical::claim_key(tx, &claim.id)?;
                            observation_receipt = Some(json!({
                                "receipt": claim,
                                "canonical_sortable_key": canonical::sortable_key(&key),
                            }));
                        }
                        pin_evidence(tx, id, self.origin(), &claim.id)?;
                        Ok(claim.id)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut result = operation.result.context("finished start has no result")?;
                result["result_receipts"] = serde_json::to_value(receipts)?;
                if let Some(observation) = observation_receipt {
                    result["runtime_observation"] = observation;
                }
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-heeb0d5b522fd owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET receipted=1,result=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&result)?],
                )?;
                Ok(true)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?
    }
    pub fn try_runtime_start_lock(&self, subject: &str) -> Result<Option<RuntimeStartLock>> {
        let directory = if self.shared_memory {
            std::env::temp_dir().join("st3-test-start-locks").join(
                smallclaims::hash::canonical_hash(&self.path.to_string_lossy())?,
            )
        } else {
            self.path.with_extension("runtime-start-locks")
        };
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(smallclaims::hash::canonical_hash(&subject)?);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(RuntimeStartLock(file)));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(error.into())
    }

    pub fn runtime_start_for(&self, subject: &str) -> Result<Option<RuntimeStartOperation>> {
        smallclaims::touched::note_read(|| format!("local-runtime-start:{subject}"));
        self.readers.get().query_row(
            /* sqlite-large-value: allow-for-now lv-h72767f72b89d owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!("SELECT * FROM local_runtime_starts WHERE subject=?1 AND ({PENDING}) ORDER BY id DESC LIMIT 1"),
            [subject], read,
        ).optional().map_err(Into::into)
    }

    pub fn pending_runtime_starts(&self) -> Result<Vec<RuntimeStartOperation>> {
        let connection = self.readers.get();
        let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-h62392feb7090 owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!(
            "SELECT * FROM local_runtime_starts WHERE {PENDING} ORDER BY id"
        ))?;
        statement
            .query_map([], read)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Point-read the exact admitted request snapshot at the physical boundary.
    /// Trim pins do not authorize launching after a repair or source retraction.
    pub(crate) fn runtime_start_request_retained(&self, expected: &ClaimRecord) -> Result<bool> {
        let connection = self.readers.get();
        let actual = retained_runtime_start_request(&connection, self.origin(), &expected.id)?;
        Ok(actual.is_some_and(|actual| same_runtime_start_request(&actual, expected)))
    }

    pub fn admit_runtime_start(
        &self,
        operation: &mut RuntimeStartOperation,
        limit: usize,
    ) -> Result<RuntimeStartAdmission> {
        if let Some(actor) = operation
            .body
            .pointer("/request/actor")
            .and_then(Value::as_str)
        {
            self.graph.ensure_principal_key(actor).map_err(|error| anyhow::anyhow!(
                "runtime-start admission refused: cannot prepare signing key for actor {actor}: {error}"
            ))?;
        }
        self.connection.batched(|tx| -> Result<RuntimeStartAdmission> {
            let selected = current_desired_row_tx(tx, &operation.subject)?;
            if selected.as_ref().map(|row| row.claim_id.as_str()) != operation.body["selected_token"].as_str() {
                return Ok(RuntimeStartAdmission::Superseded);
            }
            // Retraction and restoration can select the same token again. Bind
            // the prepared declaration to this same writer snapshot, not an
            // earlier body read whose token happened to survive an ABA change.
            let prepared = &operation.body["subject"];
            let matches = prepared["subject"].as_str() == Some(operation.subject.as_str())
                && match selected.as_ref() {
                    Some(row) => prepared["kind"].as_str() == Some(row.kind.as_str())
                        && prepared["desired"] == serde_json::from_str::<Value>(&row.body)?,
                    None => prepared["desired"].is_null(),
                };
            if !matches {
                return Ok(RuntimeStartAdmission::Superseded);
            }
            let selected = selected.map(|row| row.claim_id);
            let busy: bool = tx.query_row(
                /* sqlite-large-value: allow-for-now lv-hd9545758f491 owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!("SELECT EXISTS(SELECT 1 FROM local_runtime_starts WHERE subject=?1 AND ({PENDING}))"),
                [&operation.subject], |row| row.get(0))?;
            if busy { return Ok(RuntimeStartAdmission::SeatBusy); }
            if !operation.body["request"].is_null() {
                let request: ClaimRecord = serde_json::from_value(operation.body["request"].clone())?;
                let original = retained_runtime_start_request(tx, self.origin(), &request.id)?;
                if !original.is_some_and(|original| original.subject == request.subject
                    && original.kind == request.kind && original.actor == request.actor
                    && original.origin == request.origin && original.body == request.body)
                    || request.subject != operation.subject || request.kind != "runtime.action.requested"
                    || request.body.pointer("/fields/action").and_then(Value::as_str) != Some("restart")
                    || request.body.pointer("/evidence/0").and_then(Value::as_str)
                        != selected.as_deref() {
                    return Ok(RuntimeStartAdmission::Superseded);
                }
                let key = format!("agent-restart-attempt:{}", request.id);
                let mut fields = BTreeMap::from([
                    ("action".into(), Value::from("start")),
                    ("reason".into(), operation.body["reason"].clone()),
                    ("operation".into(), Value::from(operation.id.clone())),
                    ("runtime_id".into(), operation.body["member"]["runtime_id"].clone()),
                    ("terminal".into(), operation.body["member"]["terminal"].clone()),
                ]);
                if let Some(incarnation) = operation.body["expected_incarnation"].as_str() {
                    fields.insert("incarnation_id".into(), Value::from(incarnation));
                }
                let control = ClaimInput { subject: operation.subject.clone(), kind: "runtime.action.requested".into(),
                    actor: request.actor.clone(), fields, evidence: vec![request.id.clone()],
                    expected_subject: None, idempotency_key: Some(key.clone()) };
                let control = if request.actor.is_none() {
                    let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_observations WHERE dedupe_key=?1)",
                        [local_observation_dedupe_key(&control.kind, &key)], |row| row.get(0))?;
                    if duplicate { return Ok(RuntimeStartAdmission::SeatBusy); }
                    validate_local_observation(&control)?;
                    insert_local_observation_tx(tx, self.origin(), &control, now_ms())?.0
                } else {
                    let (key, digest) = claim_operation(&control)?.context("control attempt needs idempotency")?;
                    if operation_tx(tx, &key)?.is_some() || !smallclaims::store::checkpoint::checkpointed_operation(tx, &key)?.is_empty() {
                        return Ok(RuntimeStartAdmission::SeatBusy);
                    }
                    let body = json!({"fields":control.fields,"evidence":control.evidence,
                        "_operation":{"id":key,"request_digest":digest}});
                    let mut predecessors = latest_claim_id_tx(tx, &operation.subject)?.into_iter().collect::<Vec<_>>();
                    predecessors.push(request.id.clone());
                    predecessors.sort(); predecessors.dedup();
                    let claim = append_claim_tx(tx, self.origin(), &operation.subject, &control.kind,
                        control.actor.as_deref(), &body, &predecessors, None)?;
                    register_operation_tx(tx, &claim)?;
                    claim
                };
                operation.body["control_request"] = Value::from(control.id.clone());
                operation.body["control_request_receipt"] = serde_json::to_value(&control)?;
                pin_evidence(tx, &operation.id, self.origin(), &control.id)?;
            }
            let mut fields = BTreeMap::from([
                ("action".into(), Value::from("start")),
                ("operation".into(), Value::from(operation.id.clone())),
            ]);
            if let Some(runtime) = operation.body.pointer("/member/runtime_id").and_then(Value::as_str) {
                fields.insert("runtime_id".into(), Value::from(runtime));
            }
            if let Some(terminal) = operation.body.pointer("/member/terminal").and_then(Value::as_bool) {
                fields.insert("terminal".into(), Value::from(terminal));
            }
            if let Some(incarnation) = operation.body["expected_incarnation"].as_str() {
                fields.insert("incarnation_id".into(), Value::from(incarnation));
            }
            let request = ClaimInput { subject: operation.subject.clone(), kind: "runtime.action.requested".into(), actor: None,
                fields, evidence: selected.into_iter().chain(operation.body["control_request"].as_str().map(str::to_owned)).collect(), expected_subject: None,
                idempotency_key: Some(format!("start-request:{}", operation.id)) };
            validate_local_observation(&request)?;
            let (request, _) = insert_local_observation_tx(tx, self.origin(), &request, now_ms())?;
            operation.body["start_request"] = Value::from(request.id.clone());
            operation.body["start_request_receipt"] = serde_json::to_value(&request)?;
            operation.body["start_request_position"] = json!(local_observation_position(&request).context("physical request needs its original local position")?);
            let body = serde_json::to_string(&operation.body)?;
            tx.execute(/* sqlite-large-value: allow-for-now lv-hc03971e3c1cc owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO local_runtime_starts(id,subject,owner,body) VALUES (?1,?2,?3,?4)",
                params![operation.id, operation.subject, operation.owner, body])?;
            if let Some(request) = operation.body["start_request"].as_str() {
                pin_evidence(tx, &operation.id, self.origin(), request)?;
            }
            let running: usize = tx.query_row("SELECT COUNT(*) FROM local_runtime_starts WHERE finished=0 AND dispatched=1", [], |row| row.get(0))?;
            if running >= limit { return Ok(RuntimeStartAdmission::CapacityBusy); }
            Ok(RuntimeStartAdmission::Admitted)
        }).map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub fn dispatch_runtime_start(&self, id: &str, limit: usize) -> Result<bool> {
        self.connection.batched(|tx| -> Result<bool> {
            let running: usize = tx.query_row("SELECT COUNT(*) FROM local_runtime_starts WHERE finished=0 AND dispatched=1", [], |row| row.get(0))?;
            if running >= limit { return Ok(false); }
            Ok(tx.execute(/* sqlite-large-value: allow-for-now lv-h254bdd4d414d owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET dispatched=1 WHERE id=?1 AND dispatched=0 AND finished=0 AND revoked_by IS NULL", [id])? == 1)
        }).map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub fn runtime_start_failure(&self, subject: &str) -> Result<Option<String>> {
        smallclaims::touched::note_read(|| format!("local-runtime-start:{subject}"));
        let operation = self
            .readers
            .get()
            .query_row(
                "SELECT * FROM local_runtime_starts WHERE subject=?1 ORDER BY CAST(json_extract(body,'$.start_request_position') AS INTEGER) DESC LIMIT 1",
                [subject],
                read,
            )
            .optional()?;
        let Some(operation) = operation else {
            return Ok(None);
        };
        if !operation.finished
            || operation.revoked_by.is_some()
            || operation.body["selected_token"].as_str()
                != self.selected_desired_token(subject)?.as_deref()
        {
            return Ok(None);
        }
        Ok(operation
            .result
            .and_then(|body| body["error"].as_str().map(str::to_owned)))
    }

    pub fn runtime_start_counts(&self) -> Result<(u64, u64)> {
        self.readers.get().query_row(
            "SELECT COUNT(*), COALESCE(SUM(revoked_by IS NOT NULL),0) FROM local_runtime_starts WHERE finished=0 AND dispatched=1",
            [], |row| Ok((row.get(0)?, row.get(1)?))).map_err(Into::into)
    }

    pub fn unresolved_runtime_start_counts(&self) -> Result<(u64, u64)> {
        self.readers.get().query_row(
            /* sqlite-large-value: allow-for-now lv-hce8c6f733d92 owner=st3::store::runtime_starts TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*), COALESCE(SUM(revoked_by IS NOT NULL),0) FROM local_runtime_starts WHERE {PENDING}"),
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(Into::into)
    }

    pub fn runtime_start_revoked(&self, id: &str) -> Result<bool> {
        self.readers
            .get()
            .query_row(
                "SELECT revoked_by IS NOT NULL FROM local_runtime_starts WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn revoke_runtime_start(&self, id: &str, request: &str) -> Result<()> {
        self.connection.batched(|tx| -> Result<()> {
            tx.execute(/* sqlite-large-value: allow-for-now lv-h0bdfa9a80b9d owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET revoked_by=COALESCE(revoked_by,?2) WHERE id=?1", params![id, request])?;
            pin_evidence(tx, id, self.origin(), request)?;
            Ok(())
        })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    pub fn finish_runtime_start(&self, id: &str, result: &Value) -> Result<()> {
        self.connection.batched(|tx| -> Result<()> {
            let operation = tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
            if operation.finished { return Ok(()); }
            let mut result = result.clone();
            closed_runtime_start_result(&operation, self.origin(), &mut result)?;
            tx.execute(/* sqlite-large-value: allow-for-now lv-hc851a2f92574 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET finished=1,result=?2,
                cleanup_verified=CASE WHEN json_extract(?2,'$.error') IS NOT NULL AND json_extract(?2,'$.producer_closed')=1 THEN 1 ELSE cleanup_verified END
                WHERE id=?1 AND finished=0", params![id, serde_json::to_string(&result)?])?;
            Ok(())
        }).map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// Revocation alone cannot close a dispatched worker. Decide this under the
    /// same writer that dispatch uses, before retaining never-dispatched proof.
    pub(crate) fn cancel_undispatched_runtime_start(&self, id: &str) -> Result<bool> {
        self.connection.batched(|tx| -> Result<bool> {
            let operation = tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
            if operation.dispatched || operation.finished || operation.revoked_by.is_none() { return Ok(false); }
            let mut result = json!({"error":"start was revoked before dispatch","producer_closed":true,"closure_kind":"never-dispatched"});
            closed_runtime_start_result(&operation, self.origin(), &mut result)?;
            tx.execute(/* sqlite-large-value: allow-for-now lv-h09738fc36913 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET finished=1,cleanup_verified=1,result=?2 WHERE id=?1",
                params![id, serde_json::to_string(&result)?])?;
            Ok(true)
        }).map_err(|error| anyhow::anyhow!("{error}"))?
    }

    /// Called only after a positive exact-operation producer closure check.
    pub(crate) fn retain_runtime_start_producer_closed(&self, id: &str) -> Result<()> {
        self.connection
            .batched(|tx| -> Result<()> {
                let operation =
                    tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
                anyhow::ensure!(
                    operation.finished && operation.revoked_by.is_some(),
                    "closure needs a returned revoked operation"
                );
                let mut result = operation
                    .result
                    .clone()
                    .context("closed operation has no result")?;
                if result["error"].is_null() {
                    result["error"] =
                        Value::from("revoked producer closed without a verified launch");
                }
                result["producer_closed"] = Value::from(true);
                closed_runtime_start_result(&operation, self.origin(), &mut result)?;
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-h4db693e3ebc6 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET cleanup_verified=1,result=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&result)?],
                )?;
                release_evidence(tx, id)?;
                Ok(())
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// An exact physical ending witness is retained separately from launch success.
    pub(crate) fn retain_runtime_start_ended(
        &self,
        id: &str,
        incarnation: &str,
        status: &str,
    ) -> Result<bool> {
        anyhow::ensure!(
            !incarnation.is_empty() && matches!(status, "exited" | "stopped" | "vanished"),
            "invalid runtime ending witness"
        );
        self.connection
            .batched(|tx| -> Result<bool> {
                let operation =
                    tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
                if !operation.finished || operation.revoked_by.is_none() {
                    return Ok(false);
                }
                let mut result = operation
                    .result
                    .clone()
                    .context("ended operation has no launch result")?;
                if result["incarnation"]
                    .as_str()
                    .is_some_and(|previous| previous != incarnation)
                {
                    return Ok(false);
                }
                result["incarnation"] = Value::from(incarnation);
                result["verification"] = Value::from("operation-ended");
                result["phase"] = Value::from("ended");
                retain_runtime_start_closure(
                    &operation,
                    self.origin(),
                    &mut result,
                    json!({"kind":"exact-runtime-ended","status":status,"incarnation":incarnation}),
                )?;
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-h1569b1c2c1e8 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET cleanup_verified=1,result=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&result)?],
                )?;
                release_evidence(tx, id)?;
                Ok(true)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub(crate) fn verify_runtime_start(&self, id: &str, incarnation: &str) -> Result<bool> {
        self.connection.batched(|tx| -> Result<bool> {
            Ok(tx.execute(
                /* sqlite-large-value: allow-for-now lv-h4b506f93921b owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET result=json_set(result,
                    '$.incarnation',?2,'$.verification','operation-incarnation','$.phase','completed')
                 WHERE id=?1 AND finished=1 AND applied=0 AND receipted=0
                    AND revoked_by IS NULL AND json_extract(result,'$.error') IS NULL",
                params![id, incarnation])? == 1)
        }).map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub(crate) fn close_unverified_runtime_start(&self, id: &str) -> Result<bool> {
        self.connection
            .batched(|tx| -> Result<bool> {
                let operation =
                    tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
                if !operation.finished
                    || operation.applied
                    || operation.receipted
                    || operation.revoked_by.is_some()
                    || operation
                        .result
                        .as_ref()
                        .is_none_or(|result| !result["error"].is_null())
                {
                    return Ok(false);
                }
                let mut result = operation
                    .result
                    .clone()
                    .context("closed operation has no result")?;
                result["error"] = Value::from("producer closed without a verified launch");
                result["producer_closed"] = Value::from(true);
                closed_runtime_start_result(&operation, self.origin(), &mut result)?;
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-hdfab0238d16a owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET cleanup_verified=1,result=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&result)?],
                )?;
                Ok(true)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub fn runtime_start_applied(&self, id: &str) -> Result<()> {
        self.connection
            .batched(|tx| {
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-h45fa1142ded8 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET applied=1 WHERE id=?1 AND finished=1
                        AND (revoked_by IS NOT NULL OR json_extract(result,'$.error') IS NOT NULL
                            OR json_extract(result,'$.verification')='operation-incarnation')",
                    [id],
                )?;
                release_evidence(tx, id)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }

    /// Commit a positive, exact-incarnation physical retirement receipt and its
    /// ledger proof together. This is never called for a signal-only stop.
    pub(crate) fn apply_runtime_start_retirement(
        &self,
        id: &str,
        incarnation: &str,
        request: &str,
    ) -> Result<bool> {
        self.connection.batched(|tx| -> Result<bool> {
            let operation = tx.query_row("SELECT * FROM local_runtime_starts WHERE id=?1", [id], read)?;
            if !operation.finished || operation.revoked_by.is_none() || operation.cleanup_verified {
                return Ok(false);
            }
            let Some(position) = request.strip_prefix(&format!("{LOCAL_OBSERVATION_ID_PREFIX}{}/", self.origin())) else {
                return Ok(false);
            };
            let requested = tx.query_row(
                "SELECT id,after_store_index,subject,kind,actor,body,observed_at_unix_ms FROM local_observations WHERE id=?1",
                [position], |row| local_observation_from_row(self.origin(), row),
            ).optional()?;
            let Some(requested) = requested else { return Ok(false) };
            let member = &operation.body["member"];
            if requested.id != request || requested.subject != operation.subject || requested.kind != "runtime.action.requested"
                || requested.body["fields"]["action"] != "terminate"
                || requested.body["fields"]["runtime_id"] != member["runtime_id"]
                || requested.body["fields"]["terminal"] != member["terminal"]
                || requested.body["fields"]["incarnation_id"] != incarnation
                || requested.body["fields"]["operation"].as_str().is_some_and(|bound| bound != id) {
                return Ok(false);
            }
            let mut evidence = vec![request.into(), operation.body["start_request"].as_str().context("retirement has no launch request")?.into()];
            if let Some(control) = operation.body["control_request"].as_str() { evidence.push(control.into()); }
            let mut input = ClaimInput {
                subject: operation.subject.clone(), kind: "runtime.action.succeeded".into(), actor: None,
                fields: BTreeMap::from([
                    ("action".into(), Value::from("terminate")),
                    ("operation".into(), Value::from(id)),
                    ("runtime_id".into(), member["runtime_id"].clone()),
                    ("terminal".into(), member["terminal"].clone()),
                    ("incarnation_id".into(), Value::from(incarnation)),
                    ("operation_status".into(), Value::from("retired")),
                ]),
                evidence,
                expected_subject: None, idempotency_key: Some(format!("start-retirement:{id}:{incarnation}")),
            };
            if let Some(token) = operation.body["desired_token"].as_str() {
                input.fields.insert("desired_token".into(), Value::from(token));
            }
            validate_local_observation(&input)?;
            let receipt = if runtime_start_requests_retained(tx, self.origin(), &operation.body)? {
                Some(insert_local_observation_tx(tx, self.origin(), &input, now_ms())?.0)
            } else { None };
            let mut result = operation.result.clone().context("retired operation has no launch result")?;
            result["incarnation"] = Value::from(incarnation);
            result["verification"] = Value::from("operation-retired");
            result["phase"] = Value::from("retired");
            result["retirement_request"] = Value::from(request);
            result["retirement_result"] = serde_json::json!(receipt.map(|receipt| receipt.id));
            let witness = json!({"kind":"verified-native-retirement","incarnation":incarnation,
                "retirement_request":request,"retirement_request_receipt":requested,
                "retirement_result":result["retirement_result"]});
            retain_runtime_start_closure(&operation, self.origin(), &mut result, witness)?;
            tx.execute(/* sqlite-large-value: allow-for-now lv-h89540ce01f97 owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET cleanup_verified=1,result=?2 WHERE id=?1",
                params![id, serde_json::to_string(&result)?])?;
            release_evidence(tx, id)?;
            Ok(true)
        }).map_err(|error| anyhow::anyhow!("{error}"))?
    }

    pub fn runtime_start_cleanup_verified(&self, id: &str) -> Result<()> {
        self.connection
            .batched(|tx| {
                tx.execute(
                    /* sqlite-large-value: allow-for-now lv-h7127520d07dd owner=st3::store::runtime_starts TODO=bound-or-reference-values-and-reclamation */ "UPDATE local_runtime_starts SET cleanup_verified=1 WHERE id=?1",
                    [id],
                )?;
                release_evidence(tx, id)
            })
            .map_err(|error| anyhow::anyhow!("{error}"))??;
        Ok(())
    }
}

impl Drop for RuntimeStartLock {
    fn drop(&mut self) {
        // Explicit unlock: forked native subprocesses must not retain admission after
        // the owner's job exits. The descriptor itself is opened CLOEXEC by Rust.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(id: &str, subject: &str, token: Option<String>) -> RuntimeStartOperation {
        RuntimeStartOperation {
            id: id.into(),
            subject: subject.into(),
            owner: "owner".into(),
            body: serde_json::json!({"subject":{"subject":subject,"kind":"agent","desired":null},"selected_token": token, "desired_token":"desired-test", "member":{"runtime_id":subject,"terminal":true}}),
            revoked_by: None,
            finished: false,
            result: None,
            applied: false,
            cleanup_verified: false,
            dispatched: false,
            receipted: false,
        }
    }

    fn receipt(subject: &str) -> ClaimInput {
        ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([("action".into(), Value::from("start"))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("test-start:{subject}")),
        }
    }

    fn publish(store: &Store, source: &str, key: &str) {
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let preview = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&intent, &preview.subject_tokens, key).unwrap();
    }

    #[test]
    fn retirement_proof_requires_the_exact_subject_operation_request_and_incarnation() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        let mut admitted = operation("operation-a", subject, None);
        admitted.body["member"] = json!({"runtime_id":"example.worker", "terminal":true});
        assert_eq!(
            store.admit_runtime_start(&mut admitted, 2).unwrap(),
            RuntimeStartAdmission::Admitted
        );
        store
            .finish_runtime_start(
                &admitted.id,
                &json!({"error":"producer returned", "producer_closed":false}),
            )
            .unwrap();
        let request = |seat: &str, operation: &str, incarnation: &str, key: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: seat.into(),
                    kind: "runtime.action.requested".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), Value::from("terminate")),
                        ("operation".into(), Value::from(operation)),
                        ("runtime_id".into(), Value::from("example.worker")),
                        ("terminal".into(), Value::from(true)),
                        ("incarnation_id".into(), Value::from(incarnation)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap()
        };
        let correct = request(subject, &admitted.id, "incarnation-a", "correct-retirement");
        store
            .revoke_runtime_start(&admitted.id, &correct.id)
            .unwrap();
        store.runtime_start_applied(&admitted.id).unwrap();
        let foreign_seat = request(
            "agent/example/other",
            &admitted.id,
            "incarnation-a",
            "foreign-seat",
        );
        let foreign_operation =
            request(subject, "operation-b", "incarnation-a", "foreign-operation");
        assert!(
            !store
                .apply_runtime_start_retirement(&admitted.id, "incarnation-a", &foreign_seat.id)
                .unwrap()
        );
        assert!(
            !store
                .apply_runtime_start_retirement(
                    &admitted.id,
                    "incarnation-a",
                    &foreign_operation.id
                )
                .unwrap()
        );
        assert!(
            !store
                .apply_runtime_start_retirement(&admitted.id, "incarnation-b", &correct.id)
                .unwrap()
        );
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (1, 1));
        assert!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .apply_runtime_start_retirement(&admitted.id, "incarnation-a", &correct.id)
                .unwrap()
        );
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (0, 0));
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE id=?1",
                [&admitted.id],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["incarnation"], "incarnation-a");
        assert_eq!(result["verification"], "operation-retired");
        assert_eq!(result["retirement_request"], correct.id);
        assert_eq!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    #[ignore = "explicit isolated writer-cost probe; set ST3_START_COST_DIR"]
    fn start_lifecycle_writer_cost_probe() {
        if crate::test_support::supervise_test() {
            return;
        }
        let output = std::path::PathBuf::from(
            std::env::var_os("ST3_START_COST_DIR").expect("owned cost output directory"),
        );
        std::fs::create_dir_all(&output).unwrap();
        unsafe {
            std::env::set_var("ST3_PROFILE_DIR", &output);
            std::env::set_var("ST3_PROFILE_SLOW_MS", "0");
        }
        smallclaims::profile::init_from_env();
        for sample in 0..12 {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(&directory.path().join("claims.db"), "node").unwrap();
            let subject = "agent/example/cost";
            let mut start = operation(&format!("cost-{sample}"), subject, None);
            start.owner = "node".into();
            let before = smallclaims::sqlite::work::total();
            smallclaims::profile::task("runtime-start/writer-cost", || {
                assert_eq!(
                    store.admit_runtime_start(&mut start, 2).unwrap(),
                    RuntimeStartAdmission::Admitted
                );
                assert!(store.dispatch_runtime_start(&start.id, 2).unwrap());
                store.finish_runtime_start(&start.id, &json!({"incarnation":"actual-incarnation","verification":"operation-incarnation"})).unwrap();
                let observed = ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("status".into(), Value::from("starting")),
                        ("runtime_id".into(), Value::from(subject)),
                        ("terminal".into(), Value::from(true)),
                        ("incarnation_id".into(), Value::from("actual-incarnation")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("cost-observed:{sample}")),
                };
                assert!(
                    store
                        .apply_runtime_start_result(&start.id, &[receipt(subject), observed])
                        .unwrap()
                );
                store.runtime_start_applied(&start.id).unwrap();
            });
            let work = smallclaims::sqlite::work::total() - before;
            assert!(store.runtime_start_for(subject).unwrap().is_none());
            println!(
                "writer-cost sample={sample} statements={} vm_steps={} fullscan={} sorts={}",
                work.statements, work.vm_steps, work.fullscan_steps, work.sorts
            );
        }
        let reports = std::fs::read_to_string(output.join("slow.jsonl")).unwrap();
        let samples = reports
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| line["label"] == "runtime-start/writer-cost")
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 12);
        assert!(
            samples
                .iter()
                .all(|sample| sample["writer_acquisitions"].as_u64().unwrap() > 0)
        );
        println!(
            "writer-cost profile={}",
            output.join("slow.jsonl").display()
        );
    }

    #[test]
    fn unrelated_start_churn_cannot_starve_a_verified_incarnations_exit() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/ended";
        let mut ended = operation("ended", subject, None);
        store.admit_runtime_start(&mut ended, 2).unwrap();
        store
            .finish_runtime_start(
                &ended.id,
                &json!({"incarnation":"actual-incarnation","verification":"operation-incarnation"}),
            )
            .unwrap();
        let mut churn = operation("churn", "agent/example/churn", None);
        store.admit_runtime_start(&mut churn, 2).unwrap();
        let capture = store.capture_runtime_observations().unwrap();
        // The physical capture sees the first incarnation exit without a PID.
        let observed = ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::from("exited")),
                ("incarnation_id".into(), Value::Null),
                ("runtime_id".into(), Value::from(subject)),
                ("terminal".into(), Value::from(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        // Continuous other-seat work occurs after the read and before its writer CAS.
        store
            .connection
            .batched(|tx| -> Result<()> {
                for _ in 0..128 {
                    tx.execute(
                        "UPDATE local_runtime_starts SET body=body WHERE id='churn'",
                        [],
                    )?;
                }
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert!(
            store
                .append_reconciled_runtime_observation(&observed)
                .unwrap()
                .is_some(),
            "an exited verified incarnation must be recorded on the first pass despite unrelated churn"
        );
        assert_eq!(
            store
                .latest_claim(subject, Some("runtime.observed"))
                .unwrap()
                .unwrap()
                .body["fields"]["status"],
            "exited"
        );
        drop(capture);
        let capture = store.capture_runtime_observations().unwrap();
        store
            .connection
            .batched(|tx| -> Result<()> {
                tx.execute(
                    "UPDATE local_runtime_starts SET body=body WHERE id='ended'",
                    [],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert!(
            store
                .append_reconciled_runtime_observation(&observed)
                .unwrap()
                .is_none(),
            "same-subject mutation still rejects the pre-read witness"
        );
        drop(capture);
    }

    #[test]
    fn keyless_agent_restart_mints_its_key_or_refuses_before_any_admission() {
        for blocked in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(&directory.path().join("claims.db"), "node").unwrap();
            let actor = "agent/example/keyless";
            let request = store
                .append_claim(&ClaimInput {
                    subject: actor.into(),
                    kind: "runtime.action.requested".into(),
                    actor: Some(actor.into()),
                    fields: BTreeMap::from([("action".into(), Value::from("restart"))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let keys = directory.path().join("keys");
            if blocked {
                std::fs::write(&keys, "not a directory").unwrap();
            }
            store.use_key_directory(&keys).unwrap();
            store
                .set_node_key(Arc::new(
                    smallclaims::fleet::MemberKey::generate().unwrap().0,
                ))
                .unwrap();
            let held = |connection: &Connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM held_keys WHERE principal=?1",
                        [actor],
                        |row| row.get::<_, u64>(0),
                    )
                    .unwrap()
            };
            assert_eq!(held(&store.readers.get()), 0);
            let mut start = operation("keyless-agent", actor, None);
            start.body["request"] = serde_json::to_value(&request).unwrap();
            let admitted = store.admit_runtime_start(&mut start, 2);
            if blocked {
                assert!(
                    format!("{:#}", admitted.unwrap_err())
                        .contains("cannot prepare signing key for actor agent/example/keyless")
                );
                assert!(store.pending_runtime_starts().unwrap().is_empty());
                assert_eq!(held(&store.readers.get()), 0);
                assert!(
                    store
                        .observations_for(actor, "runtime.action.requested")
                        .unwrap()
                        .iter()
                        .all(|claim| claim.body["fields"]["action"] != "start")
                );
                assert_eq!(
                    store
                        .claims_for(actor, Some("runtime.action.requested"))
                        .unwrap()
                        .len(),
                    1
                );
            } else {
                assert_eq!(admitted.unwrap(), RuntimeStartAdmission::Admitted);
                assert_eq!(held(&store.readers.get()), 1);
                let control: ClaimRecord =
                    serde_json::from_value(start.body["control_request_receipt"].clone()).unwrap();
                assert_eq!(control.actor.as_deref(), Some(actor));
                assert!(store.runtime_start_for(actor).unwrap().is_some());
            }
        }
    }

    #[test]
    fn requester_attempt_and_physical_request_commit_together_after_declaration_cas() {
        for changed in [false, true] {
            let store = Store::open_memory("node").unwrap();
            let subject = "agent/example/worker";
            publish(
                &store,
                "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
                "restart-declaration",
            );
            let token = store.selected_desired_token(subject).unwrap();
            let mut start = operation("restart-bound", subject, token.clone());
            start.body["subject"] = serde_json::to_value(
                store
                    .desired_subjects_named(&[subject.into()])
                    .unwrap()
                    .remove(0),
            )
            .unwrap();
            let request = store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.action.requested".into(),
                    actor: Some("person/example".into()),
                    fields: BTreeMap::from([("action".into(), Value::from("restart"))]),
                    evidence: token.into_iter().collect(),
                    expected_subject: None,
                    idempotency_key: Some("restart-request".into()),
                })
                .unwrap();
            start.body["request"] = serde_json::to_value(&request).unwrap();
            if changed {
                publish(
                    &store,
                    "version 2\nstop \"agent/example/worker\"",
                    "restart-superseded",
                );
            }
            let admitted = store.admit_runtime_start(&mut start, 2).unwrap();
            if changed {
                assert_eq!(admitted, RuntimeStartAdmission::Superseded);
                assert!(
                    store
                        .observations_for(subject, "runtime.action.requested")
                        .unwrap()
                        .iter()
                        .all(|claim| claim.body["fields"]["action"] != "start")
                );
                assert_eq!(
                    store
                        .claims_for(subject, Some("runtime.action.requested"))
                        .unwrap()
                        .len(),
                    1
                );
                assert!(store.pending_runtime_starts().unwrap().is_empty());
                continue;
            }
            assert_eq!(admitted, RuntimeStartAdmission::Admitted);
            let control: ClaimRecord =
                serde_json::from_value(start.body["control_request_receipt"].clone()).unwrap();
            assert_eq!(control.actor, request.actor);
            assert_eq!(control.body["fields"]["operation"], start.id);
            assert_eq!(control.body["evidence"], json!([request.id]));
            assert!(control.operation_id.is_some());
            store.finish_runtime_start(&start.id,&json!({"incarnation":"new-incarnation", "verification":"operation-incarnation"})).unwrap();
            assert!(
                store
                    .apply_runtime_start_result(&start.id, &[receipt(subject)])
                    .unwrap()
            );
            let completed = store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .remove(0);
            assert!(
                completed.body["evidence"]
                    .as_array()
                    .unwrap()
                    .contains(&start.body["start_request"])
            );
            assert!(
                completed.body["evidence"]
                    .as_array()
                    .unwrap()
                    .contains(&start.body["control_request"])
            );
            assert!(store.runtime_start_request_retained(&control).unwrap());
            store
                .connection
                .batched(|tx| -> Result<()> {
                    tx.execute(
                        "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                        [&control.id],
                    )?;
                    Ok(())
                })
                .unwrap()
                .unwrap();
            assert!(
                !store.runtime_start_request_retained(&control).unwrap(),
                "unchanged repaired body must not grant launch"
            );
        }
    }

    #[test]
    fn completion_preserves_running_incarnation_and_retains_its_canonical_observation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        let store = Store::open(&path, "node").unwrap();
        let subject = "agent/example/worker";
        let mut start = operation("operation-observed", subject, None);
        store.admit_runtime_start(&mut start, 2).unwrap();
        store.finish_runtime_start(&start.id, &json!({"incarnation":"actual-incarnation", "verification":"operation-incarnation"})).unwrap();
        let observed = |status: &str, incarnation: &str| ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("status".into(), Value::from(status)),
                ("runtime_id".into(), Value::from(subject)),
                ("terminal".into(), Value::from(true)),
                ("incarnation_id".into(), Value::from(incarnation)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        store
            .append_claim(&observed("stopped", "prior-incarnation"))
            .unwrap();
        let running = store
            .append_claim(&observed("running", "actual-incarnation"))
            .unwrap();
        let key = canonical::sortable_key(
            &canonical::claim_key(&store.readers.get(), &running.id).unwrap(),
        );
        assert!(
            store
                .apply_runtime_start_result(
                    &start.id,
                    &[receipt(subject), observed("starting", "actual-incarnation")]
                )
                .unwrap()
        );
        assert_eq!(
            store
                .latest_claim(subject, Some("runtime.observed"))
                .unwrap()
                .unwrap()
                .id,
            running.id
        );
        store.runtime_start_applied(&start.id).unwrap();
        store
            .connection
            .batched(|tx| -> Result<()> {
                tx.execute("DELETE FROM claims WHERE id=?1", [&running.id])?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        drop(store);
        let store = Store::open(&path, "node").unwrap();
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE id=?1",
                [&start.id],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result["runtime_observation"]["receipt"],
            serde_json::to_value(running).unwrap()
        );
        assert_eq!(
            result["runtime_observation"]["canonical_sortable_key"],
            serde_json::to_value(key).unwrap()
        );
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (0, 0));
    }

    #[test]
    fn cancellation_proof_requires_undispatched_admission_and_survives_trim_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        let store = Store::open(&path, "node").unwrap();
        let mut dispatched = operation("dispatched", "agent/example/live", None);
        store.admit_runtime_start(&mut dispatched, 1).unwrap();
        assert!(store.dispatch_runtime_start(&dispatched.id, 1).unwrap());
        let mut queued = operation("queued", "agent/example/queued", None);
        assert_eq!(
            store.admit_runtime_start(&mut queued, 1).unwrap(),
            RuntimeStartAdmission::CapacityBusy
        );
        store
            .revoke_runtime_start(&queued.id, "cancel-queued")
            .unwrap();
        assert!(store.cancel_undispatched_runtime_start(&queued.id).unwrap());
        store.runtime_start_applied(&queued.id).unwrap();
        store
            .revoke_runtime_start(&dispatched.id, "cancel-dispatched")
            .unwrap();
        assert!(
            !store
                .cancel_undispatched_runtime_start(&dispatched.id)
                .unwrap()
        );
        assert!(
            store
                .runtime_start_for(&dispatched.subject)
                .unwrap()
                .is_some()
        );
        store.trim_local_observations(now_ms() + 1, 1, 0).unwrap();
        drop(store);
        let store = Store::open(&path, "node").unwrap();
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE id='queued'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        let proof = &result["closure_proof"];
        assert_eq!(proof["verification"], "producer-closed");
        assert_eq!(proof["witness"]["kind"], "never-dispatched");
        assert_eq!(
            proof["start_request_receipt"]["id"],
            queued.body["start_request"]
        );
        assert_eq!(
            result["closure_proof_id"],
            format!("start-proof/{}", canonical_hash(proof).unwrap())
        );
        assert!(store.runtime_start_for(&queued.subject).unwrap().is_none());
    }

    #[test]
    fn already_ended_cleanup_retains_an_exact_incarnation_witness() {
        let store = Store::open_memory("node").unwrap();
        let mut start = operation("ended", "agent/example/worker", None);
        start.body["expected_incarnation"] = Value::from("prior-incarnation");
        store.admit_runtime_start(&mut start, 2).unwrap();
        store.finish_runtime_start(&start.id, &json!({"incarnation":"actual-incarnation", "verification":"operation-incarnation"})).unwrap();
        store
            .revoke_runtime_start(&start.id, "cancel-ended")
            .unwrap();
        store.runtime_start_applied(&start.id).unwrap();
        assert!(
            !store
                .retain_runtime_start_ended(&start.id, "foreign-incarnation", "exited")
                .unwrap()
        );
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (1, 1));
        assert!(
            store
                .retain_runtime_start_ended(&start.id, "actual-incarnation", "exited")
                .unwrap()
        );
        let result: String = store
            .readers
            .get()
            .query_row(
                "SELECT result FROM local_runtime_starts WHERE id=?1",
                [&start.id],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["closure_proof"]["verification"], "operation-ended");
        assert_eq!(result["closure_proof"]["incarnation"], "actual-incarnation");
        assert_eq!(
            result["closure_proof"]["expected_incarnation"],
            "prior-incarnation"
        );
        assert_eq!(result["closure_proof"]["witness"]["status"], "exited");
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (0, 0));
    }

    #[test]
    fn verified_start_receipts_bind_writer_proof_and_refuse_conflicting_inputs() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        let mut start = operation("operation-a", subject, None);
        start.body["expected_incarnation"] = Value::from("prior-incarnation");
        store.admit_runtime_start(&mut start, 2).unwrap();
        store.finish_runtime_start(&start.id, &json!({"incarnation":"actual-incarnation", "verification":"operation-incarnation"})).unwrap();
        for (field, value) in [
            ("operation", "another-operation"),
            ("incarnation_id", "another-incarnation"),
            ("runtime_id", "another-runtime"),
            ("desired_token", "another-declaration"),
        ] {
            let mut wrong = receipt(subject);
            wrong.fields.insert(field.into(), Value::from(value));
            assert!(
                !store
                    .apply_runtime_start_result(&start.id, &[wrong])
                    .unwrap()
            );
        }
        assert!(
            !store
                .apply_runtime_start_result(&start.id, &[receipt("agent/example/other")])
                .unwrap()
        );
        assert!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .apply_runtime_start_result(&start.id, &[receipt(subject)])
                .unwrap()
        );
        let completed = store
            .observations_for(subject, "runtime.action.succeeded")
            .unwrap()
            .remove(0);
        assert_eq!(completed.body["fields"]["operation_status"], "verified");
        assert_eq!(completed.body["fields"]["operation"], start.id);
        assert_eq!(completed.body["fields"]["runtime_id"], subject);
        assert_eq!(completed.body["fields"]["terminal"], true);
        assert_eq!(
            completed.body["fields"]["incarnation_id"],
            "actual-incarnation"
        );
        assert_eq!(completed.body["fields"]["desired_token"], "desired-test");
        let requested = store
            .observations_for(subject, "runtime.action.requested")
            .unwrap()
            .remove(0);
        assert_eq!(requested.body["fields"]["terminal"], true);
        assert_eq!(
            requested.body["fields"]["incarnation_id"],
            "prior-incarnation"
        );
        assert!(
            completed.body["evidence"]
                .as_array()
                .unwrap()
                .contains(&Value::from(requested.id))
        );
    }

    #[test]
    fn ordinary_failed_starts_cannot_publish_success_or_forge_closure_status() {
        for closed in [false, true] {
            let store = Store::open_memory("node").unwrap();
            let subject = "agent/example/worker";
            let mut start = operation("operation-a", subject, None);
            store.admit_runtime_start(&mut start, 2).unwrap();
            store.finish_runtime_start(&start.id, &json!({"error":"producer returned", "producer_closed":closed, "verification":if closed {"producer-closed"} else {"producer-open-unverified"}})).unwrap();
            assert!(
                !store
                    .apply_runtime_start_result(&start.id, &[receipt(subject)])
                    .unwrap()
            );
            let mut failed = receipt(subject);
            failed.kind = "runtime.action.failed".into();
            failed
                .fields
                .insert("operation_status".into(), Value::from("producer-closed"));
            assert!(
                store
                    .apply_runtime_start_result(&start.id, &[failed])
                    .unwrap()
            );
            let completed = store
                .observations_for(subject, "runtime.action.failed")
                .unwrap()
                .remove(0);
            assert_eq!(
                completed.body["fields"]["operation_status"],
                if closed {
                    Value::from("producer-closed")
                } else {
                    Value::Null
                }
            );
            assert_eq!(completed.body["fields"]["operation"], start.id);
            assert_eq!(completed.body["fields"]["runtime_id"], subject);
            assert!(
                store
                    .observations_for(subject, "runtime.action.succeeded")
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn declaration_and_revocation_fence_admission_and_completion_atomically() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        publish(
            &store,
            "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
            "initial",
        );
        let token = store.selected_desired_token(subject).unwrap();
        let desired = serde_json::to_value(
            store
                .desired_subjects_named(&[subject.into()])
                .unwrap()
                .remove(0),
        )
        .unwrap();
        let mut first = operation("one", subject, token.clone());
        first.body["subject"] = desired.clone();
        assert_eq!(
            store.admit_runtime_start(&mut first.clone(), 2).unwrap(),
            RuntimeStartAdmission::Admitted
        );
        assert!(store.dispatch_runtime_start("one", 2).unwrap());
        store
            .finish_runtime_start("one", &serde_json::json!({"incarnation": "one"}))
            .unwrap();
        store
            .revoke_runtime_start("one", "durable-stop-request")
            .unwrap();
        assert!(
            !store
                .apply_runtime_start_result("one", &[receipt(subject)])
                .unwrap()
        );
        assert!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
        store.runtime_start_applied("one").unwrap();
        store.runtime_start_cleanup_verified("one").unwrap();
        let mut second = operation("two", subject, token.clone());
        second.body["subject"] = desired;
        assert_eq!(
            store.admit_runtime_start(&mut second.clone(), 2).unwrap(),
            RuntimeStartAdmission::Admitted
        );
        store
            .finish_runtime_start("two", &serde_json::json!({"incarnation": "two"}))
            .unwrap();
        publish(&store, "version 2\nstop \"agent/example/worker\"", "stop");
        assert!(
            !store
                .apply_runtime_start_result("two", &[receipt(subject)])
                .unwrap()
        );
        assert_eq!(
            store
                .admit_runtime_start(&mut operation("three", "agent/example/new", token), 2)
                .unwrap(),
            RuntimeStartAdmission::Superseded
        );
        assert!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn accepted_completion_receipts_are_idempotent_before_context_is_applied() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        let start = operation("one", subject, None);
        store.admit_runtime_start(&mut start.clone(), 2).unwrap();
        store
            .finish_runtime_start("one", &serde_json::json!({"incarnation": null, "verification": "launch-accepted-unverified"}))
            .unwrap();
        assert!(
            !store
                .apply_runtime_start_result("one", &[receipt(subject)])
                .unwrap()
        );
        assert!(store.runtime_start_for(subject).unwrap().is_some());
        assert!(store.verify_runtime_start("one", "one").unwrap());
        assert!(
            store
                .apply_runtime_start_result("one", &[receipt(subject)])
                .unwrap()
        );
        assert!(
            store
                .apply_runtime_start_result("one", &[receipt(subject)])
                .unwrap()
        );
        assert_eq!(
            store
                .observations_for(subject, "runtime.action.succeeded")
                .unwrap()
                .len(),
            1
        );
        assert!(store.runtime_start_for(subject).unwrap().is_some());
        store.runtime_start_applied("one").unwrap();
        assert!(store.runtime_start_for(subject).unwrap().is_none());
    }

    #[test]
    fn an_ordinary_backend_error_does_not_release_launch_admission() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        let mut start = operation("one", subject, None);
        store.admit_runtime_start(&mut start, 2).unwrap();
        store
            .finish_runtime_start(
                "one",
                &json!({"error":"producer unverified", "producer_closed":false}),
            )
            .unwrap();
        store.runtime_start_applied("one").unwrap();
        assert!(store.runtime_start_for(subject).unwrap().is_some());
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (1, 0));
        assert_eq!(
            store
                .admit_runtime_start(&mut operation("two", subject, None), 2)
                .unwrap(),
            RuntimeStartAdmission::SeatBusy
        );
        store.runtime_start_cleanup_verified("one").unwrap();
        assert!(store.runtime_start_for(subject).unwrap().is_none());
        assert_eq!(store.unresolved_runtime_start_counts().unwrap(), (0, 0));
    }

    #[test]
    fn the_kernel_releases_nonblocking_admission_when_its_owner_process_dies() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        let store = Store::open(&path, "node").unwrap();
        let subject = "agent/example/worker";
        drop(store.try_runtime_start_lock(subject).unwrap().unwrap());
        let lock_path = std::ffi::CString::new(
            path.with_extension("runtime-start-locks")
                .join(smallclaims::hash::canonical_hash(&subject).unwrap())
                .as_os_str()
                .as_bytes(),
        )
        .unwrap();
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Only async-signal-safe syscalls after fork of the threaded Store owner.
            unsafe {
                libc::close(pipe[0]);
                let fd = libc::open(lock_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                let acquired =
                    u8::from(fd >= 0 && libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0);
                libc::write(pipe[1], (&acquired as *const u8).cast(), 1);
                libc::close(pipe[1]);
                loop {
                    libc::pause();
                }
            }
        }
        struct Child(libc::pid_t);
        impl Drop for Child {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
        let child = Child(pid);
        unsafe {
            libc::close(pipe[1]);
        }
        let mut acquired = 0u8;
        assert_eq!(
            unsafe { libc::read(pipe[0], (&mut acquired as *mut u8).cast(), 1) },
            1
        );
        unsafe {
            libc::close(pipe[0]);
        }
        assert_eq!(acquired, 1);
        assert!(store.try_runtime_start_lock(subject).unwrap().is_none());
        drop(child);
        assert!(store.try_runtime_start_lock(subject).unwrap().is_some());
    }

    #[test]
    fn trimming_preserves_unresolved_start_and_revocation_evidence() {
        let store = Store::open_memory("node").unwrap();
        let subject = "agent/example/worker";
        let request = |action: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.action.requested".into(),
                    actor: None,
                    fields: BTreeMap::from([("action".into(), Value::from(action))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        let mut prepared = operation("one", subject, None);
        store.admit_runtime_start(&mut prepared, 2).unwrap();
        let start = prepared.body["start_request"].as_str().unwrap().to_owned();
        let revoke = request("terminate");
        store.revoke_runtime_start("one", &revoke.id).unwrap();
        for _ in 0..8 {
            request("start");
        }
        store
            .finish_runtime_start("one", &serde_json::json!({"error": "producer unverified"}))
            .unwrap();
        store.runtime_start_applied("one").unwrap();
        store.trim_local_observations(now_ms() + 1, 1, 2).unwrap();
        let retained = store
            .observations_for(subject, "runtime.action.requested")
            .unwrap();
        assert!(retained.iter().any(|r| r.id == start));
        assert!(retained.iter().any(|r| r.id == revoke.id));
        store.runtime_start_cleanup_verified("one").unwrap();
        store.trim_local_observations(now_ms() + 1, 1, 2).unwrap();
        let retained = store
            .observations_for(subject, "runtime.action.requested")
            .unwrap();
        assert!(!retained.iter().any(|r| r.id == start || r.id == revoke.id));
        let resolved = store
            .readers
            .get()
            .query_row(
                "SELECT * FROM local_runtime_starts WHERE id='one'",
                [],
                read,
            )
            .unwrap();
        assert!(resolved.finished && resolved.applied && resolved.cleanup_verified);
        assert_eq!(resolved.body["start_request"], start);
        assert_eq!(resolved.revoked_by.as_deref(), Some(revoke.id.as_str()));
        assert_eq!(resolved.result.unwrap()["error"], "producer unverified");
    }

    fn format_cuts(connection: &Connection) -> (i64, i64) {
        let read_cut = |key| {
            connection
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1",
                    [key],
                    |row| row.get(0),
                )
                .unwrap()
        };
        (
            read_cut("publication-operation-format-shared-cut"),
            read_cut("publication-operation-format-local-cut"),
        )
    }

    #[test]
    fn format_cuts_precede_bound_requests_and_survive_replay_forget_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(format_cuts(&store.readers.get()), (0, 0));
        publish(
            &store,
            "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
            "legacy",
        );
        store
            .append_claim(&receipt("agent/example/worker"))
            .unwrap();
        drop(store);
        let legacy = Connection::open(&path).unwrap();
        let shared: i64 = legacy
            .query_row("SELECT MAX(store_index) FROM claims", [], |row| row.get(0))
            .unwrap();
        let local: i64 = legacy
            .query_row("SELECT MAX(id) FROM local_observations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(shared > 0 && local > 0);
        legacy.execute_batch(
            "DROP TABLE local_runtime_start_evidence; DROP TABLE local_runtime_starts; DROP TABLE local_runtime_start_observation_subjects; DROP TABLE local_runtime_start_observation_generation;
             DELETE FROM meta WHERE key IN ('publication-operation-format-shared-cut','publication-operation-format-local-cut');
             PRAGMA user_version=18;",
        ).unwrap();
        drop(legacy);
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(format_cuts(&store.readers.get()), (shared, local));
        let subject = "agent/example/worker";
        let token = store.selected_desired_token(subject).unwrap();
        let mut start = operation("modern-operation", subject, token);
        start.body["subject"] = serde_json::to_value(
            store
                .desired_subjects_named(&[subject.into()])
                .unwrap()
                .remove(0),
        )
        .unwrap();
        assert_eq!(
            store.admit_runtime_start(&mut start, 2).unwrap(),
            RuntimeStartAdmission::Admitted
        );
        let request_position: i64 = store
            .readers
            .get()
            .query_row("SELECT MAX(id) FROM local_observations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(request_position > local);
        let generation = |connection: &Connection| {
            connection
                .query_row(
                    "SELECT value FROM local_runtime_start_observation_generation WHERE id=1",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap()
        };
        let captured_generation = generation(&store.readers.get());
        assert!(captured_generation > 0);
        publish(
            &store,
            "version 2\nstop \"agent/example/worker\"",
            "modern-stop",
        );
        {
            let mut writer = store.connection.write();
            let transaction = writer.transaction().unwrap();
            replay_graph_from_nothing_tx(&transaction).unwrap();
            assert_eq!(format_cuts(&transaction), (shared, local));
            assert_eq!(generation(&transaction), captured_generation);
            transaction.commit().unwrap();
        }
        store.forget_current_views();
        assert_eq!(format_cuts(&store.readers.get()), (shared, local));
        assert_eq!(generation(&store.readers.get()), captured_generation);
        drop(store);
        let reopened = Store::open(&path, "node").unwrap();
        assert_eq!(format_cuts(&reopened.readers.get()), (shared, local));
        assert_eq!(generation(&reopened.readers.get()), captured_generation);
        assert_eq!(
            reopened.runtime_start_for(subject).unwrap().unwrap().body["start_request"],
            start.body["start_request"]
        );
    }

    #[test]
    fn missing_format_cut_with_existing_operations_refuses_a_later_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        let store = Store::open(&path, "node").unwrap();
        store
            .admit_runtime_start(&mut operation("one", "agent/example/worker", None), 2)
            .unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "DELETE FROM meta WHERE key='publication-operation-format-local-cut'",
                [],
            )
            .unwrap();
        drop(connection);
        let error = match Store::open(&path, "node") {
            Ok(_) => panic!("must not capture a boundary after bound requests exist"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("lacks immutable publication format cuts"));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key='publication-operation-format-local-cut'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM local_runtime_starts", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            1
        );
    }

    #[test]
    fn schema_18_upgrade_and_reopen_preserve_unresolved_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.db");
        drop(Store::open(&path, "node").unwrap());
        let old = Connection::open(&path).unwrap();
        old.execute_batch("DROP TABLE local_runtime_starts; PRAGMA user_version=18;")
            .unwrap();
        drop(old);
        let store = Store::open(&path, "node").unwrap();
        let subject = "agent/example/worker";
        store
            .admit_runtime_start(&mut operation("one", subject, None), 2)
            .unwrap();
        store.dispatch_runtime_start("one", 2).unwrap();
        store
            .revoke_runtime_start("one", "durable-stop-request")
            .unwrap();
        drop(store);
        let reopened = Store::open(&path, "node").unwrap();
        let operation = reopened.runtime_start_for(subject).unwrap().unwrap();
        assert_eq!(
            operation.revoked_by.as_deref(),
            Some("durable-stop-request")
        );
        assert!(operation.dispatched && !operation.finished);
        assert_eq!(reopened.runtime_start_counts().unwrap(), (1, 1));
        let version: u32 = reopened
            .readers
            .get()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, smallclaims::store::CURRENT_SCHEMA_VERSION);
        assert_eq!(version, st3_schema::STORAGE_VERSION);
    }
}

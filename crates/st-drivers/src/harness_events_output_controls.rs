//! Test-only atomic output capture prototype over an isolated native event spool.
//! The parent includes this module only under cfg(test). Production timeline
//! writes, adapters, daemon publication and health callers are unchanged.
//! Authored callback witnesses are not native/protocol or physical authority proof.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::output_model::{CAPABILITY, Snapshot, component};
use crate::harness_timeline::{Operation, Record};

const SNAPSHOT_KIND: &str = "harness-output";
const MAX_BATCH: usize = 128;
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Proposed producer-callback input, authored directly by these controls.
/// A future adapter must establish its provenance before normalization/queueing.
/// Hydration/replay supplies no progress. Unknown raw tool IDs remain incomplete
/// even if timeline normalization has replaced them with an opaque pseudonym.
pub(crate) struct OutputProgress<'a> {
    pub operation: &'a Operation,
    pub original_at_ms: u64,
    pub body_changed: bool,
    pub tool_identity_complete: bool,
}

pub(crate) struct OutputBatch<'a> {
    pub runtime_incarnation: &'a str,
    pub provider_incarnation: &'a str,
    pub ownership_sequence: u64,
    pub driver: &'a str,
    pub component: &'a str,
    pub capability: &'a str,
    /// A loss, unsupported callback or ambiguous raw tool identity can be observed
    /// even when timeline deduplication emitted no new operation. Never clears debt.
    pub capture_gap: bool,
    pub progress: &'a [OutputProgress<'a>],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Envelope {
    // The existing event outbox resolves the runtime from this provider token.
    incarnation: String,
    // The native drain classifies the source before dispatching its event kind.
    // A future output publication arm is still required before any caller is wired.
    driver: String,
    runtime_incarnation: String,
    output: Snapshot,
}

/// Keep provider claim locking outside the SQLite writer, matching state writers.
/// Every state/runtime/sequence check and both mutations share one native writer
/// transaction. Failure anywhere leaves timeline, output and outbox unchanged.
pub(crate) fn write_timeline_with_output(
    agent_dir: &Path,
    record: &Record,
    new_operations: &[Operation],
    batch: &OutputBatch<'_>,
) -> Result<()> {
    anyhow::ensure!(
        crate::contracts::schema_matches(&record.schema, "st.harness-timeline.v1")
            && !batch.runtime_incarnation.is_empty()
            && !batch.provider_incarnation.is_empty()
            && batch.ownership_sequence > 0
            && batch.capability == CAPABILITY
            && component(batch.driver) == Some(batch.component)
            && record.driver == batch.driver
            && record.incarnation_id == batch.provider_incarnation
            && record.next_sequence > 0
            && record.operations.len() <= MAX_BATCH
            && new_operations.len() <= MAX_BATCH
            && batch.progress.len() <= MAX_BATCH,
        "native output needs an exact bounded producer identity"
    );
    anyhow::ensure!(
        new_operations.iter().all(|operation| {
            operation.driver == batch.driver
                && operation.incarnation_id == batch.provider_incarnation
                && !operation.entry_id.is_empty()
                && operation.sequence > 0
                && operation.revision > 0
                && operation.sequence < record.next_sequence
                && operation
                    .source_id
                    .as_deref()
                    .is_some_and(|source| source.starts_with("source/") && source.len() <= 64)
                && record
                    .operations
                    .iter()
                    .any(|retained| retained == operation)
        }) && batch.progress.iter().all(|progress| {
            progress.original_at_ms > 0
                && new_operations
                    .iter()
                    .any(|operation| operation == progress.operation)
        }),
        "output progress must belong to the same admitted timeline batch"
    );
    new_operations
        .iter()
        .try_fold(0_usize, |bytes, operation| {
            let operation_bytes = serde_json::to_vec(operation)?.len();
            let total = bytes
                .checked_add(operation_bytes)
                .ok_or_else(|| anyhow::anyhow!("native output batch byte count overflow"))?;
            anyhow::ensure!(total <= MAX_BATCH_BYTES, "native output batch is too wide");
            Ok::<_, anyhow::Error>(total)
        })?;
    crate::harness_state::with_current_ownership(
        agent_dir,
        batch.provider_incarnation,
        batch.ownership_sequence,
        || {
            let mut connection = super::open(agent_dir)?;
            let tx =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let raw: Vec<u8> = tx.query_row(
                "SELECT body FROM snapshots WHERE kind='harness-state'",
                [],
                |row| row.get(0),
            )?;
            let state: Value = serde_json::from_slice(&raw)?;
            anyhow::ensure!(
                state["incarnation"].as_str() == Some(batch.provider_incarnation)
                    && state["harness"].as_str() == Some(batch.driver)
                    && state["seq"].as_u64() == Some(batch.ownership_sequence)
                    && matches!(
                        state["state"].as_str(),
                        Some("ready" | "idle" | "active" | "child")
                    )
                    && state.get("exit").is_none_or(Value::is_null),
                "native output provider state is unavailable or terminal"
            );
            let (runtime, bound): (String, String) = tx.query_row(
                "SELECT (SELECT value FROM metadata WHERE key='runtime'), value
                 FROM metadata WHERE key=?1",
                [format!("provider-runtime:{}", batch.provider_incarnation)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            anyhow::ensure!(
                runtime == batch.runtime_incarnation && bound == runtime,
                "native output runtime binding was superseded"
            );
            let previous: Option<Envelope> = tx
                .query_row(
                    "SELECT body FROM snapshots WHERE kind=?1",
                    [SNAPSHOT_KIND],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?
                .map(|raw| serde_json::from_slice(&raw))
                .transpose()?;
            let mut envelope = Envelope {
                incarnation: batch.provider_incarnation.into(),
                driver: batch.driver.into(),
                runtime_incarnation: runtime,
                output: Snapshot::new(
                    batch.driver,
                    batch.provider_incarnation,
                    batch.ownership_sequence,
                )
                .ok_or_else(|| anyhow::anyhow!("unsupported native output producer"))?,
            };
            if let Some(previous) = previous.as_ref() {
                anyhow::ensure!(
                    previous.output.ownership_sequence <= batch.ownership_sequence,
                    "native output ownership cannot rewind"
                );
                if previous.incarnation == envelope.incarnation
                    && previous.runtime_incarnation == envelope.runtime_incarnation
                    && previous.output.ownership_sequence == batch.ownership_sequence
                {
                    anyhow::ensure!(
                        previous.driver == batch.driver
                            && previous.output.owns(
                                batch.driver,
                                batch.provider_incarnation,
                                batch.ownership_sequence
                            ),
                        "native output snapshot ownership is malformed"
                    );
                    envelope.output = previous.output.clone();
                }
            }
            if batch.capture_gap {
                envelope.output.gap();
            }
            // Capture the preceding entries once under the same native writer.
            // A revision/final flag is not output progress even if an adapter
            // incorrectly says the payload changed. This is not live provenance:
            // replay/hydration must still arrive without a progress witness.
            let mut preceding = preceding_operations(&tx, batch)?;
            let progress_by_entry = batch
                .progress
                .iter()
                .map(|progress| {
                    let operation = progress.operation;
                    (
                        (
                            operation.entry_id.as_str(),
                            operation.sequence,
                            operation.revision,
                        ),
                        progress,
                    )
                })
                .collect::<BTreeMap<_, _>>();
            anyhow::ensure!(
                progress_by_entry.len() == batch.progress.len(),
                "duplicate native progress witness"
            );
            for operation in new_operations {
                let source = operation.source_id.as_deref().unwrap();
                if let Some(progress) = progress_by_entry.get(&(
                    operation.entry_id.as_str(),
                    operation.sequence,
                    operation.revision,
                )) {
                    if !progress.tool_identity_complete
                        && matches!(operation.entry_type.as_str(), "tool_call" | "tool_result")
                    {
                        envelope.output.gap();
                    } else {
                        envelope.output.observe(
                            operation,
                            Some(progress.original_at_ms),
                            progress.body_changed
                                && payload_changed(preceding.get(source), operation),
                        );
                    }
                }
                preceding.insert(source.into(), operation.clone());
            }
            if !new_operations.is_empty() {
                super::append_timeline_operations(&tx, record, new_operations)?;
            }
            if previous.as_ref() != Some(&envelope) {
                tx.execute(
                    "INSERT INTO snapshots VALUES (?1,?2)
                     ON CONFLICT(kind) DO UPDATE SET body=excluded.body",
                    params![SNAPSHOT_KIND, serde_json::to_vec(&envelope)?],
                )?;
                super::append_event(&tx, SNAPSHOT_KIND, &serde_json::to_value(&envelope)?)?;
            }
            tx.commit()?;
            super::signal_wake(agent_dir);
            Ok(())
        },
    )
}

fn preceding_operations(
    tx: &rusqlite::Transaction<'_>,
    batch: &OutputBatch<'_>,
) -> Result<BTreeMap<String, Operation>> {
    let sources = batch
        .progress
        .iter()
        .map(|progress| {
            // Progress references only operations validated above.
            progress.operation.source_id.as_deref().unwrap()
        })
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut query = tx.prepare(
        "SELECT source,body FROM timeline WHERE id IN (
            SELECT MAX(id) FROM timeline WHERE incarnation=?1
            AND source IN (SELECT value FROM json_each(?2)) GROUP BY source)",
    )?;
    query
        .query_map(
            params![batch.provider_incarnation, serde_json::to_string(&sources)?],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .map(|row| {
            let (source, body) = row?;
            Ok((source, serde_json::from_str(&body)?))
        })
        .collect()
}

fn payload_changed(previous: Option<&Operation>, current: &Operation) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.role != current.role || previous.entry_type != current.entry_type {
        return true;
    }
    if current.entry_type == "content" {
        return previous.body.get("text") != current.body.get("text");
    }
    previous.body != current.body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer, claim};

    fn prepare(root: &Path, driver: &'static str) -> (Writer, Record, Operation, u64) {
        super::super::enable(root, "runtime-a").unwrap();
        let seq = claim(root, "agent/example", driver, "provider-a").unwrap();
        // Required by the live-observation writer. This label is an authored
        // private fixture input, not a checked physical PTY/process witness.
        let mut writer = Writer::new(
            root,
            "agent/example",
            driver,
            Some("private-control-pty".into()),
        )
        .with_ownership("provider-a", seq);
        writer
            .observe(Observation::new(
                Activity::Active,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        let operation = Operation {
            operation: "append".into(),
            entry_id: "native-entry".into(),
            sequence: 1,
            revision: 1,
            role: "assistant".into(),
            entry_type: "content".into(),
            final_entry: true,
            body: serde_json::json!({"text":"native output"}),
            driver: driver.into(),
            incarnation_id: "provider-a".into(),
            observed_at_unix_ms: 100,
            source_id: Some("source/native".into()),
        };
        let record = Record {
            schema: "st.harness-timeline.v1".into(),
            driver: driver.into(),
            incarnation_id: "provider-a".into(),
            next_sequence: 2,
            operations: vec![operation.clone()],
        };
        (writer, record, operation, seq)
    }

    fn batch<'a>(
        record: &'a Record,
        seq: u64,
        progress: &'a [OutputProgress<'a>],
    ) -> OutputBatch<'a> {
        OutputBatch {
            runtime_incarnation: "runtime-a",
            provider_incarnation: &record.incarnation_id,
            ownership_sequence: seq,
            driver: &record.driver,
            component: component(&record.driver).unwrap(),
            capability: CAPABILITY,
            capture_gap: false,
            progress,
        }
    }

    // Compare all affected table contents, including timeline-next and outbox
    // byte counters. A mere queue length check would miss partial metadata writes.
    fn spool_image(root: &Path) -> Vec<Vec<String>> {
        let connection = super::super::open(root).unwrap();
        [
            "SELECT json_array(key,value) FROM metadata ORDER BY key",
            "SELECT json_array(kind,hex(body)) FROM snapshots ORDER BY kind",
            "SELECT json_array(id,incarnation,source,body) FROM timeline ORDER BY id",
            "SELECT json_array(sequence,runtime_incarnation,queued_at_ms,kind,body) FROM events ORDER BY sequence",
        ]
        .into_iter()
        .map(|sql| {
            connection
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        })
        .collect()
    }

    // Compare persisted shared effects. Queue time and outbox sequence belong to
    // each independent spool; output events intentionally consume extra sequence
    // numbers, so timeline event ordering and content are compared instead.
    fn shared_timeline_image(root: &Path) -> Vec<Vec<String>> {
        let connection = super::super::open(root).unwrap();
        [
            "SELECT json_array(key,value) FROM metadata WHERE key LIKE 'timeline-next:%' ORDER BY key",
            "SELECT json_array(id,incarnation,source,body) FROM timeline ORDER BY id",
            "SELECT json_array(runtime_incarnation,kind,body) FROM events WHERE kind='harness-timeline' ORDER BY sequence",
        ]
        .into_iter()
        .map(|sql| {
            connection
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        })
        .collect()
    }

    fn pending_bytes(root: &Path) -> u64 {
        super::super::open(root)
            .unwrap()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='pending-bytes'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn output_event_bytes(root: &Path) -> u64 {
        super::super::open(root)
            .unwrap()
            .query_row(
                "SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM events WHERE kind=?1",
                [SNAPSHOT_KIND],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn production_and_private_writes_match_shared_timeline_effects() {
        for driver in ["codex", "claude", "pi", "omp", "opencode"] {
            let production = tempfile::tempdir().unwrap();
            let private = tempfile::tempdir().unwrap();
            let (_, _, _, production_seq) = prepare(production.path(), driver);
            let (_, mut record, mut operation, seq) = prepare(private.path(), driver);
            assert_eq!(production_seq, seq);
            let production_before = pending_bytes(production.path());
            let private_before = pending_bytes(private.path());
            let production_state = super::super::read_snapshot(production.path(), "harness-state")
                .unwrap()
                .unwrap();
            let private_state = super::super::read_snapshot(private.path(), "harness-state")
                .unwrap()
                .unwrap();

            // First append then finalization of unchanged content: both actual
            // writer entry points process the identical submitted operation.
            for revision in [1, 2] {
                operation.revision = revision;
                operation.final_entry = revision == 2;
                operation.operation = if revision == 1 { "append" } else { "finalize" }.into();
                record.operations = vec![operation.clone()];
                super::super::write_timeline(
                    production.path(),
                    &record,
                    std::slice::from_ref(&operation),
                )
                .unwrap();
                let progress = [OutputProgress {
                    operation: &operation,
                    original_at_ms: revision * 10,
                    body_changed: true,
                    tool_identity_complete: true,
                }];
                write_timeline_with_output(
                    private.path(),
                    &record,
                    std::slice::from_ref(&operation),
                    &batch(&record, seq, &progress),
                )
                .unwrap();
                assert_eq!(
                    shared_timeline_image(production.path()),
                    shared_timeline_image(private.path()),
                    "{driver}: revision {revision}"
                );
                assert_eq!(
                    pending_bytes(production.path()) - production_before,
                    pending_bytes(private.path())
                        - private_before
                        - output_event_bytes(private.path()),
                    "{driver}: shared outbox byte accounting at revision {revision}"
                );
            }
            assert_eq!(
                super::super::read_snapshot(production.path(), "harness-state")
                    .unwrap()
                    .unwrap(),
                production_state
            );
            assert_eq!(
                super::super::read_snapshot(private.path(), "harness-state")
                    .unwrap()
                    .unwrap(),
                private_state
            );
            assert!(
                super::super::read_snapshot(production.path(), SNAPSHOT_KIND)
                    .unwrap()
                    .is_none()
            );
            let image: Envelope = serde_json::from_slice(
                &super::super::read_snapshot(private.path(), SNAPSHOT_KIND)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(image.output.last_output.unwrap().at_unix_ms, 10);
        }
    }

    #[test]
    fn superseded_token_is_refused_by_production_and_private_timeline_writes() {
        for private in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (_, mut record, mut operation, old_seq) = prepare(root.path(), "omp");
            let successor = claim(root.path(), "agent/example", "omp", "provider-b").unwrap();
            assert!(successor > old_seq);
            let mut writer = Writer::new(
                root.path(),
                "agent/example",
                "omp",
                Some("private-control-pty".into()),
            )
            .with_ownership("provider-b", successor);
            writer
                .observe(Observation::new(
                    Activity::Active,
                    BlockedOn::None,
                    InputBuffer::Unknown,
                ))
                .unwrap();
            assert_eq!(
                super::super::current_token(&super::super::open(root.path()).unwrap())
                    .unwrap()
                    .as_deref(),
                Some("provider-b")
            );
            let before = spool_image(root.path());
            let result = if private {
                write_timeline_with_output(
                    root.path(),
                    &record,
                    std::slice::from_ref(&operation),
                    &batch(&record, old_seq, &[]),
                )
            } else {
                super::super::write_timeline(root.path(), &record, std::slice::from_ref(&operation))
            };
            assert!(
                result.is_err(),
                "private={private}: retired owner must be refused"
            );
            assert_eq!(spool_image(root.path()), before);

            // An accepted successor prevents blanket refusal from satisfying the
            // negative control; no native callback provenance is inferred.
            record.incarnation_id = "provider-b".into();
            operation.incarnation_id = "provider-b".into();
            record.operations = vec![operation.clone()];
            if private {
                write_timeline_with_output(
                    root.path(),
                    &record,
                    std::slice::from_ref(&operation),
                    &batch(&record, successor, &[]),
                )
                .unwrap();
            } else {
                super::super::write_timeline(
                    root.path(),
                    &record,
                    std::slice::from_ref(&operation),
                )
                .unwrap();
            }
            assert_eq!(
                super::super::read_timeline(root.path())
                    .unwrap()
                    .unwrap()
                    .operations,
                vec![operation]
            );
        }
    }

    #[test]
    fn all_harnesses_commit_original_output_time_with_timeline_and_outbox() {
        for driver in ["codex", "claude", "pi", "omp", "opencode"] {
            let root = tempfile::tempdir().unwrap();
            let (_, record, operation, seq) = prepare(root.path(), driver);
            let progress = [OutputProgress {
                operation: &operation,
                original_at_ms: 10,
                body_changed: true,
                tool_identity_complete: true,
            }];
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &progress),
            )
            .unwrap();
            let image: Envelope = serde_json::from_slice(
                &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(image.output.last_output.unwrap().at_unix_ms, 10);
            assert!(
                !image.output.tool_coverage_complete,
                "{driver}: output alone is not complete tool coverage"
            );
            let pending = super::super::pending(root.path(), 10).unwrap();
            assert_eq!(pending[pending.len() - 2].kind, "harness-timeline");
            assert_eq!(pending.last().unwrap().kind, SNAPSHOT_KIND);
            assert_eq!(pending.last().unwrap().payload["driver"], driver);
            assert!(
                pending
                    .iter()
                    .all(|event| event.runtime_incarnation == "runtime-a")
            );
        }
    }

    #[test]
    fn a_failed_output_write_rolls_back_timeline_and_all_outbox_changes() {
        let root = tempfile::tempdir().unwrap();
        let (_, record, operation, seq) = prepare(root.path(), "codex");
        let before = spool_image(root.path());
        super::super::open(root.path()).unwrap().execute_batch(
            "CREATE TRIGGER reject_output BEFORE INSERT ON snapshots WHEN NEW.kind='harness-output'
             BEGIN SELECT RAISE(ABORT,'output failure'); END;"
        ).unwrap();
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &progress)
            )
            .is_err()
        );
        assert_eq!(spool_image(root.path()), before);
        assert!(super::super::read_timeline(root.path()).unwrap().is_none());
        assert!(
            super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_failed_output_event_rolls_back_snapshot_timeline_and_spool_metadata() {
        let root = tempfile::tempdir().unwrap();
        let (_, record, operation, seq) = prepare(root.path(), "codex");
        let before = spool_image(root.path());
        super::super::open(root.path()).unwrap().execute_batch(
            "CREATE TRIGGER reject_output_event BEFORE INSERT ON events WHEN NEW.kind='harness-output'
             BEGIN SELECT RAISE(ABORT,'output event failure'); END;"
        ).unwrap();
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &progress)
            )
            .is_err()
        );
        assert_eq!(spool_image(root.path()), before);
    }

    #[test]
    fn wrong_runtime_sequence_driver_and_terminal_state_cannot_publish_output() {
        let root = tempfile::tempdir().unwrap();
        let (mut writer, record, operation, seq) = prepare(root.path(), "omp");
        let before = super::super::pending(root.path(), 10).unwrap().len();
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        let mut wrong = batch(&record, seq, &progress);
        wrong.runtime_incarnation = "runtime-b";
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &wrong
            )
            .is_err()
        );
        wrong = batch(&record, seq + 1, &progress);
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &wrong
            )
            .is_err()
        );
        wrong = batch(&record, seq, &progress);
        wrong.driver = "pi";
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &wrong
            )
            .is_err()
        );
        // Keep the requested tuple intact while changing the authoritative raw
        // provider record. The check must occur inside the actual transaction.
        let connection = super::super::open(root.path()).unwrap();
        let raw = super::super::read_snapshot(root.path(), "harness-state")
            .unwrap()
            .unwrap();
        let mut raw: Value = serde_json::from_slice(&raw).unwrap();
        raw["harness"] = "pi".into();
        connection
            .execute(
                "UPDATE snapshots SET body=?1 WHERE kind='harness-state'",
                [serde_json::to_vec(&raw).unwrap()],
            )
            .unwrap();
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &progress)
            )
            .is_err()
        );
        raw["harness"] = "omp".into();
        connection
            .execute(
                "UPDATE snapshots SET body=?1 WHERE kind='harness-state'",
                [serde_json::to_vec(&raw).unwrap()],
            )
            .unwrap();
        assert_eq!(
            super::super::pending(root.path(), 10).unwrap().len(),
            before
        );
        writer
            .observe(
                Observation::new(Activity::Ended, BlockedOn::None, InputBuffer::Unknown)
                    .with_exit("completed"),
            )
            .unwrap();
        let terminal_events = super::super::pending(root.path(), 10).unwrap().len();
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &progress)
            )
            .is_err()
        );
        assert_eq!(
            super::super::pending(root.path(), 10).unwrap().len(),
            terminal_events
        );
        let mut ended: Value = serde_json::from_slice(
            &super::super::read_snapshot(root.path(), "harness-state")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        for stamp in [1, u64::MAX] {
            ended["writtenAtMs"] = stamp.into();
            connection
                .execute(
                    "UPDATE snapshots SET body=?1 WHERE kind='harness-state'",
                    [serde_json::to_vec(&ended).unwrap()],
                )
                .unwrap();
            assert!(
                write_timeline_with_output(
                    root.path(),
                    &record,
                    std::slice::from_ref(&operation),
                    &batch(&record, seq, &progress)
                )
                .is_err()
            );
            assert_eq!(
                super::super::pending(root.path(), 10).unwrap().len(),
                terminal_events
            );
        }
        assert!(super::super::read_timeline(root.path()).unwrap().is_none());
        assert!(
            super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_identity_or_oversized_batch_is_refused_without_spool_mutation() {
        let root = tempfile::tempdir().unwrap();
        let (_, mut record, mut operation, seq) = prepare(root.path(), "omp");
        let before = super::super::pending(root.path(), 10).unwrap();
        record.schema = "unrecognized".into();
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &[])
            )
            .is_err()
        );
        record.schema = "st.harness-timeline.v1".into();
        operation.body = serde_json::json!({"text": "x".repeat(MAX_BATCH_BYTES)});
        record.operations = vec![operation.clone()];
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &[])
            )
            .is_err()
        );
        let after = super::super::pending(root.path(), 10).unwrap();
        assert_eq!(after.len(), before.len());
        assert!(super::super::read_timeline(root.path()).unwrap().is_none());
        assert!(
            super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn duplicate_progress_witness_rolls_back_before_any_output_or_timeline_event() {
        let root = tempfile::tempdir().unwrap();
        let (_, record, operation, seq) = prepare(root.path(), "codex");
        let before = super::super::pending(root.path(), 10).unwrap().len();
        let witness = || OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        };
        let duplicated = [witness(), witness()];
        assert!(
            write_timeline_with_output(
                root.path(),
                &record,
                std::slice::from_ref(&operation),
                &batch(&record, seq, &duplicated)
            )
            .is_err()
        );
        assert_eq!(
            super::super::pending(root.path(), 10).unwrap().len(),
            before
        );
        assert!(super::super::read_timeline(root.path()).unwrap().is_none());
        assert!(
            super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn hydration_and_same_session_ownership_promotion_do_not_refresh_output() {
        let root = tempfile::tempdir().unwrap();
        let (_, mut record, mut operation, seq) = prepare(root.path(), "claude");
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &[]),
        )
        .unwrap();
        let image = || -> Envelope {
            serde_json::from_slice(
                &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(image().output.last_output, None);
        operation.sequence = 2;
        operation.entry_id = "live-entry".into();
        operation.source_id = Some("source/live".into());
        record.next_sequence = 3;
        record.operations = vec![operation.clone()];
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &progress),
        )
        .unwrap();
        assert_eq!(image().output.last_output.unwrap().at_unix_ms, 10);
        let successor = claim(root.path(), "agent/example", "claude", "provider-a").unwrap();
        let mut writer = Writer::new(
            root.path(),
            "agent/example",
            "claude",
            Some("private-control-pty".into()),
        )
        .with_ownership("provider-a", successor);
        writer
            .observe(Observation::new(
                Activity::Active,
                BlockedOn::None,
                InputBuffer::Unknown,
            ))
            .unwrap();
        operation.sequence = 3;
        operation.entry_id = "historical-entry".into();
        operation.source_id = Some("source/historical".into());
        record.next_sequence = 4;
        record.operations = vec![operation.clone()];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, successor, &[]),
        )
        .unwrap();
        assert_eq!(image().output.ownership_sequence, successor);
        assert_eq!(image().output.last_output, None);
        assert!(!image().output.tool_coverage_complete);
    }

    #[test]
    fn an_unknown_raw_tool_id_cannot_clear_a_retained_unanswered_call() {
        let root = tempfile::tempdir().unwrap();
        let (_, mut record, mut operation, seq) = prepare(root.path(), "omp");
        operation.entry_type = "tool_call".into();
        operation.body = serde_json::json!({"call_id":"call/opaque", "name":"tool"});
        record.operations = vec![operation.clone()];
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &progress),
        )
        .unwrap();
        operation.role = "tool".into();
        operation.entry_type = "tool_result".into();
        operation.entry_id = "result-entry".into();
        operation.sequence = 2;
        record.next_sequence = 3;
        record.operations = vec![operation.clone()];
        let unknown = [OutputProgress {
            operation: &operation,
            original_at_ms: 20,
            body_changed: true,
            tool_identity_complete: false,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &unknown),
        )
        .unwrap();
        let image: Envelope = serde_json::from_slice(
            &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(image.output.unresolved_tools.get("call/opaque"), Some(&10));
        assert_eq!(image.output.last_output.unwrap().at_unix_ms, 10);
        assert_eq!(image.output.progress_run_since_ms, None);
        assert!(!image.output.tool_coverage_complete);
    }

    #[test]
    fn finalization_only_cannot_refresh_the_original_output_clock() {
        let root = tempfile::tempdir().unwrap();
        let (_, mut record, mut operation, seq) = prepare(root.path(), "codex");
        operation.final_entry = false;
        record.operations = vec![operation.clone()];
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &progress),
        )
        .unwrap();
        let previous: Envelope = serde_json::from_slice(
            &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let output_events = super::super::pending(root.path(), 10)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == SNAPSHOT_KIND)
            .count();
        operation.revision = 2;
        operation.operation = "finalize".into();
        operation.final_entry = true;
        record.operations = vec![operation.clone()];
        let mistaken = [OutputProgress {
            operation: &operation,
            original_at_ms: 200,
            body_changed: true,
            tool_identity_complete: true,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &mistaken),
        )
        .unwrap();
        let finalized: Envelope = serde_json::from_slice(
            &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(previous, finalized);
        assert_eq!(finalized.output.last_output.unwrap().at_unix_ms, 10);
        let events = super::super::pending(root.path(), 10).unwrap();
        assert_eq!(events.last().unwrap().kind, "harness-timeline");
        assert_eq!(
            events
                .into_iter()
                .filter(|event| event.kind == SNAPSHOT_KIND)
                .count(),
            output_events
        );
    }

    #[test]
    fn a_capture_gap_without_a_new_timeline_entry_is_durable_and_retains_output() {
        let root = tempfile::tempdir().unwrap();
        let (_, record, operation, seq) = prepare(root.path(), "pi");
        let progress = [OutputProgress {
            operation: &operation,
            original_at_ms: 10,
            body_changed: true,
            tool_identity_complete: true,
        }];
        write_timeline_with_output(
            root.path(),
            &record,
            std::slice::from_ref(&operation),
            &batch(&record, seq, &progress),
        )
        .unwrap();
        let previous = super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
            .unwrap()
            .unwrap();
        let before: Envelope = serde_json::from_slice(&previous).unwrap();
        let timeline = super::super::read_timeline(root.path()).unwrap().unwrap();
        let mut gap = batch(&record, seq, &[]);
        gap.capture_gap = true;
        write_timeline_with_output(root.path(), &record, &[], &gap).unwrap();
        let after: Envelope = serde_json::from_slice(
            &super::super::read_snapshot(root.path(), SNAPSHOT_KIND)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after.output.last_output, before.output.last_output);
        assert_eq!(after.output.progress_run_since_ms, None);
        assert!(!after.output.tool_coverage_complete);
        assert_eq!(
            super::super::read_timeline(root.path())
                .unwrap()
                .unwrap()
                .operations,
            timeline.operations
        );
        assert_eq!(
            super::super::pending(root.path(), 10)
                .unwrap()
                .last()
                .unwrap()
                .kind,
            SNAPSHOT_KIND
        );
    }
}

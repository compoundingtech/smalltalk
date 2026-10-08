//! A channel binding is a connection capability, not authority to replace a live session.
use super::*;
use crate::mailbox::{Authority, Fence};
use crate::model::{LaunchSpec, MemberKind, MemberSpec};

#[cfg(test)]
thread_local! {
    static AFTER_FAULT_ACQUISITION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

fn lease(
    connection: &Connection,
    fence: &Fence,
) -> Result<Option<(Fence, Authority, bool)>, St3Error> {
    connection
        .prepare_cached(
            "SELECT incarnation,token,epoch,provider,session,sequence,pid,process_token,revoked
         FROM local_mailbox_leases WHERE subject=?1 AND component=?2",
        )
        .map_err(internal)?
        .query_row(params![fence.subject, fence.component], |row| {
            Ok((
                Fence {
                    subject: fence.subject.clone(),
                    component: fence.component.clone(),
                    incarnation: row.get(0)?,
                    token: row.get(1)?,
                    epoch: row.get(2)?,
                },
                Authority {
                    provider: row.get(3)?,
                    session: row.get(4)?,
                    sequence: row.get(5)?,
                    pid: row.get(6)?,
                    process_token: row.get::<_, String>(7)?.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                },
                row.get(8)?,
            ))
        })
        .optional()
        .map_err(internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(sequence: u64) -> Authority {
        Authority {
            provider: "omp".into(),
            session: format!("provider-{sequence}"),
            sequence,
            pid: std::process::id(),
            process_token: st_runtime::process_start_token(std::process::id()).unwrap(),
        }
    }
    fn request() -> Fence {
        Fence::new("agent/eval.worker", "current", "delivery")
    }
    fn declare(store: &Store) {
        let intent = crate::graph::parse_intent("version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; harness \"omp\" {} }", "node").unwrap();
        store.apply_internal(&intent, "lease-fixture").unwrap();
    }
    fn fixture() -> Store {
        let store = Store::open_memory("node").unwrap();
        declare(&store);
        crate::mailbox::tests::ready(&store, "current");
        store
    }

    #[test]
    fn fault_capture_keeps_exact_recovery_and_runtime_fences_at_each_cut() {
        let store = fixture();
        let subject = "agent/eval.worker";
        let subjects = vec![subject.into(), "agent/absent".into(), subject.into()];
        let failure = |reason: &str| store.append_claim(&ClaimInput {
            subject: subject.into(), kind: "operational.failure".into(),
            actor: Some("daemon/runtime".into()),
            fields: serde_json::from_value(json!({"condition":"mailbox-channel-lost",
                "incarnation":"current", "reason":reason, "reviewer":subject,
                "title":"Mailbox lost", "severity":"error", "targets":[subject]})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let first = failure("first loss");
        let first_cut = store.index().unwrap();
        assert_eq!(store.mailbox_faults_for(&subjects, first_cut).unwrap(),
            BTreeMap::from([(subject.into(), "first loss".into())]));
        store.append_claim(&ClaimInput {
            subject: subject.into(), kind: "operational.recovered".into(),
            actor: Some("daemon/runtime".into()),
            fields: serde_json::from_value(json!({"failure":first.id, "reason":"replay consumed"})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        assert!(store.mailbox_faults_for(&subjects, store.index().unwrap()).unwrap().is_empty());
        assert_eq!(store.mailbox_faults_for(&subjects, first_cut).unwrap()[subject], "first loss");
        failure("second loss");
        let second_cut = store.index().unwrap();
        assert_eq!(store.mailbox_faults_for(&subjects, second_cut).unwrap()[subject], "second loss",
            "an old recovery must not quiet a new interruption");
        crate::mailbox::tests::ready(&store, "replacement");
        assert!(store.mailbox_faults_for(&subjects, store.index().unwrap()).unwrap().is_empty());
        assert_eq!(store.mailbox_faults_for(&subjects, second_cut).unwrap()[subject], "second loss");
        assert!(store.mailbox_faults_for(&[], second_cut).unwrap().is_empty());
        let before = store.index().unwrap();
        let budget = smallclaims::read_budget::ReadBudget::new("expired-card-cut", std::time::Duration::ZERO);
        let error = smallclaims::read_budget::with(Some(budget), ||
            store.mailbox_faults_for(&subjects, second_cut)).unwrap_err();
        assert_eq!(error.downcast_ref::<St3Error>().unwrap().code, "read-deadline");
        assert_eq!(store.index().unwrap(), before, "failure cannot backfill or return a partial fault map");
    }

    #[test]
    fn fault_capture_refuses_failed_acquisition_without_waiting_and_preserves_a_pin() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("graph.db");
        let saved = root.path().join("graph.saved");
        let store = Arc::new(Store::open(&path, "node").unwrap());
        let subjects = vec!["agent/absent".into()];
        // A surviving pinned snapshot must not be discarded to open another reader.
        store.read_snapshot(|cut| -> Result<()> {
            let held = std::mem::take(&mut *store.readers.idle.lock().unwrap());
            std::fs::rename(&path, &saved)?;
            let result = store.mailbox_faults_for(&subjects, cut);
            std::fs::rename(&saved, &path)?;
            drop(held);
            assert!(result?.is_empty());
            Ok(())
        }).unwrap();
        // Outside a pin, deliberately make opening impossible with every idle reader
        // retained. Observe a finite result before returning any reader to the pool.
        let held = std::mem::take(&mut *store.readers.idle.lock().unwrap());
        std::fs::rename(&path, &saved).unwrap();
        let worker_store = store.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert!(worker_store.mailbox_faults_for(&subjects, 0).is_err());
            // The acquisition has already failed. Cancel exactly at its result boundary,
            // so an early outer `?` would miss the authoritative typed deadline check.
            let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed = reached.clone();
            AFTER_FAULT_ACQUISITION.with(|pause| {
                *pause.borrow_mut() = Some(Box::new(move || {
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    smallclaims::read_budget::current().unwrap().cancel();
                }));
            });
            let expired = worker_store.mailbox_faults_for(&subjects, 0).unwrap_err();
            sent.send(reached.load(std::sync::atomic::Ordering::SeqCst)
                && expired.downcast_ref::<St3Error>().is_some_and(|error| error.code == "read-deadline")).unwrap();
        });
        let result = received.recv_timeout(std::time::Duration::from_secs(1));
        std::fs::rename(&saved, &path).unwrap();
        // Release readers even on failure, so the old waiting implementation cannot
        // leave a hung control thread after the finite observation fails.
        store.readers.idle.lock().unwrap().extend(held);
        store.readers.returned.notify_all();
        worker.join().unwrap();
        assert_eq!(result.unwrap(), true, "failed acquisition must refuse promptly");
    }

    #[test]
    fn live_duplicates_and_pid_reuse_cannot_replace_or_receive_the_lease() {
        let store = fixture();
        let authority = authority(1);
        let request = request();
        let bound = store
            .bind_mailbox_with_lease(&request, Some(&authority))
            .unwrap();
        for _ in 0..3 {
            let duplicate = self::request();
            assert_eq!(
                store
                    .bind_mailbox_with_lease(&duplicate, Some(&authority))
                    .unwrap_err()
                    .code,
                "stale-mailbox-session"
            );
            let bindings: u64 = store
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM local_mailbox_bindings", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(bindings, 1);
            store.check_mailbox(&bound).unwrap();
        }
        let mut reused = authority.clone();
        reused.process_token += 1;
        assert!(
            store
                .bind_mailbox_with_lease(&request, Some(&reused))
                .is_err()
        );
        assert!(store.repair_mailbox(&bound, &reused).is_err());
        assert_eq!(
            store
                .bind_mailbox_with_lease(&request, Some(&authority))
                .unwrap()
                .epoch,
            bound.epoch
        );
    }

    #[test]
    fn uncertain_process_probes_preserve_the_canonical_capability() {
        let store = fixture();
        let owner = authority(1);
        let bound = store
            .bind_mailbox_with_lease(&request(), Some(&owner))
            .unwrap();
        let uncertain = [
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::Other)),
            anyhow::anyhow!("malformed process start token"),
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound)),
        ];
        for error in uncertain {
            let result = channel_liveness(&owner, Err(error), || false);
            assert_eq!(result, ChannelLiveness::Indeterminate);
            let connection = store.connection.lock().unwrap();
            assert!(admit_with_probe(&connection, &request(), &owner, |_| result).is_err());
            let (held, _, _) = lease(&connection, &bound).unwrap().unwrap();
            assert_eq!(
                (&held.token, held.epoch, &held.incarnation),
                (&bound.token, bound.epoch, &bound.incarnation)
            );
            let (epoch, count): (u64, u64) = connection.query_row(
                "SELECT epoch,(SELECT COUNT(*) FROM local_mailbox_bindings) FROM local_mailbox_owners",
                [], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            assert_eq!((epoch, count), (bound.epoch, 1));
        }
        let missing = channel_liveness(
            &owner,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()),
            || true,
        );
        let reused = channel_liveness(&owner, Ok(owner.process_token + 1), || false);
        for result in [missing, reused] {
            assert_eq!(result, ChannelLiveness::Dead);
            assert!(
                admit_with_probe(
                    &store.connection.lock().unwrap(),
                    &request(),
                    &owner,
                    |_| result
                )
                .is_ok()
            );
        }
        store.check_mailbox(&bound).unwrap();
    }

    #[test]
    fn malformed_persisted_process_birth_cannot_prove_a_dead_canonical_owner() {
        let store = fixture();
        let owner = authority(1);
        let bound = store
            .bind_mailbox_with_lease(&request(), Some(&owner))
            .unwrap();
        store.connection.write().execute(
            "UPDATE local_mailbox_leases SET process_token='malformed' WHERE subject=?1 AND component=?2",
            params![bound.subject,bound.component],
        ).unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&owner))
                .is_err()
        );
        assert!(store.repair_mailbox(&bound, &owner).is_err());
        let held: (String, u64) = store
            .readers
            .get()
            .query_row(
                "SELECT token,epoch FROM local_mailbox_leases WHERE subject=?1 AND component=?2",
                params![bound.subject, bound.component],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(held, (bound.token, bound.epoch));
    }

    #[test]
    fn host_move_fences_reads_and_receipts_before_runtime_reconciliation() {
        let store = fixture();
        let owner = authority(1);
        let fence = store
            .bind_mailbox_with_lease(&request(), Some(&owner))
            .unwrap();
        let moved = crate::graph::parse_intent(
            "version 2\nagent \"eval.worker\" { host \"other-node\"; workspace \"/tmp\"; harness \"omp\" {} }", "node").unwrap();
        store.apply_internal(&moved, "host-move-fixture").unwrap();
        assert_eq!(
            store.check_mailbox(&fence).unwrap_err().code,
            "stale-mailbox-session"
        );
        let receipt = ClaimInput {
            subject: "message/host-move".into(),
            kind: "message.staged".into(),
            actor: Some(fence.subject.clone()),
            fields: BTreeMap::from([("status".into(), json!("staged"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        };
        assert_eq!(
            store
                .append_mailbox_receipt(&receipt, &fence)
                .unwrap_err()
                .code,
            "stale-mailbox-session"
        );
        assert!(store.repair_mailbox(&fence, &owner).is_err());
        let runtime = store.latest_actual_value(&fence.subject).unwrap().unwrap();
        let fields = runtime.get("fields").unwrap_or(&runtime);
        assert_eq!(fields["status"], "running");
        assert_eq!(fields["incarnation_id"], "current");
    }

    #[test]
    fn repair_retains_exact_capability_but_never_rewinds_a_successor_or_revocation() {
        let store = fixture();
        let first = authority(1);
        let bound = store
            .bind_mailbox_with_lease(&request(), Some(&first))
            .unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM local_mailbox_owners", [])
            .unwrap();
        assert!(store.check_mailbox(&bound).is_err());
        assert!(store.repair_mailbox(&bound, &first).unwrap());
        assert!(!store.repair_mailbox(&bound, &first).unwrap());
        store.check_mailbox(&bound).unwrap();
        let successor = store
            .bind_mailbox_with_lease(&request(), Some(&authority(2)))
            .unwrap();
        assert_eq!(successor.epoch, bound.epoch + 1);
        assert!(store.repair_mailbox(&bound, &first).is_err());
        assert!(store.bind_mailbox_with_lease(&bound, Some(&first)).is_err());
        store.check_mailbox(&successor).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM local_mailbox_bindings WHERE token=?1",
                [&successor.token],
            )
            .unwrap();
        assert!(store.repair_mailbox(&successor, &authority(2)).is_err());
        let mut unbound = successor.clone();
        unbound.epoch = 0;
        assert!(
            store
                .bind_mailbox_with_lease(&unbound, Some(&authority(2)))
                .is_err(),
            "a revoked token cannot allocate again"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dead_channel_successor_preserves_provider_session_and_retires_old_capability() {
        let store = fixture();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut predecessor = authority(1);
        predecessor.pid = child.id();
        predecessor.process_token = st_runtime::process_start_token(child.id()).unwrap();
        let old = store
            .bind_mailbox_with_lease(&request(), Some(&predecessor))
            .unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(1)))
                .is_err()
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let new = store
            .bind_mailbox_with_lease(&request(), Some(&authority(1)))
            .unwrap();
        assert_eq!(new.epoch, old.epoch + 1);
        assert!(store.repair_mailbox(&old, &predecessor).is_err());
        assert!(
            store
                .bind_mailbox_with_lease(&old, Some(&predecessor))
                .is_err()
        );
        store.check_mailbox(&new).unwrap();
    }

    #[test]
    fn persisted_canonical_lease_survives_store_reopen_and_rejects_completed_runtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.sqlite");
        let bound = {
            let store = Store::open(&path, "node").unwrap();
            declare(&store);
            crate::mailbox::tests::ready(&store, "current");
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(1)))
                .unwrap()
        };
        let store = Store::open(&path, "node").unwrap();
        assert_eq!(
            store
                .bind_mailbox_with_lease(&bound, Some(&authority(1)))
                .unwrap()
                .epoch,
            bound.epoch
        );
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(1)))
                .is_err()
        );
        store
            .append_claim(&ClaimInput {
                subject: bound.subject.clone(),
                kind: "runtime.observed".into(),
                actor: Some(bound.subject.clone()),
                fields: BTreeMap::from([
                    ("status".into(), json!("exited")),
                    ("incarnation_id".into(), json!("current")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(store.repair_mailbox(&bound, &authority(1)).is_err());
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(1)))
                .is_err()
        );
    }

    #[test]
    fn bootstrap_promotion_keeps_capability_and_cannot_undo_provider_takeover() {
        let store = fixture();
        let intent = crate::graph::parse_intent("version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; harness \"codex\" {} }", "node").unwrap();
        store
            .apply_internal(&intent, "codex-bootstrap-fixture")
            .unwrap();
        let mut bootstrap = authority(1);
        bootstrap.provider = "codex".into();
        bootstrap.session = "runtime:current".into();
        bootstrap.sequence = 0;
        let bound = store
            .bind_mailbox_with_lease(&request(), Some(&bootstrap))
            .unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&bootstrap))
                .is_err()
        );
        let mut provider = authority(1);
        provider.provider = "codex".into();
        let mut wrong_birth = provider.clone();
        wrong_birth.process_token += 1;
        assert!(
            store
                .promote_current_mailbox_ownership(&bound, &wrong_birth)
                .is_err()
        );
        store.promote_current_mailbox_ownership(&bound, &provider).unwrap();
        assert_eq!(
            store.mailbox_lease_authority(&bound).unwrap(),
            Some(provider.clone())
        );
        let reconnect = store
            .bind_mailbox_with_lease(&bound, Some(&provider))
            .unwrap();
        assert_eq!(
            (reconnect.token, reconnect.epoch),
            (bound.token.clone(), bound.epoch)
        );
        assert!(
            store
                .bind_mailbox_with_lease(&bound, Some(&bootstrap))
                .is_err()
        );
        let mut successor = authority(2);
        successor.provider = "codex".into();
        let new = store
            .bind_mailbox_with_lease(&request(), Some(&successor))
            .unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&bound, Some(&provider))
                .is_err()
        );
        store.promote_current_mailbox_ownership(&bound, &provider).unwrap();
        assert_eq!(
            store.mailbox_lease_authority(&new).unwrap(),
            Some(successor)
        );
        store.check_mailbox(&new).unwrap();
    }

    fn declare_argv(store: &Store, host: &str) -> MemberSpec {
        let intent = crate::graph::parse_intent(
            &format!("version 2\nagent \"eval.worker\" {{ host \"{host}\"; workspace \"/tmp\"; argv \"python3\" \"probe\"; }}"),
            "node",
        ).unwrap();
        store.apply_internal(&intent, "argv-monitor-fixture").unwrap();
        store.desired_subjects_named(&["agent/eval.worker".into()]).unwrap().remove(0).member.unwrap()
    }

    #[test]
    fn argv_channel_binding_retains_runtime_declaration_and_receipt_fences() {
        let store = fixture();
        let member = declare_argv(&store, "node");
        let bound = store.bind_argv_mailbox_checked(&request(), &member, &|| Ok(())).unwrap();
        assert!(store.mailbox_lease_authority(&bound).unwrap().is_none());
        let reconnect = store.bind_argv_mailbox_checked(&bound, &member, &|| Ok(())).unwrap();
        assert_eq!((reconnect.token, reconnect.epoch), (bound.token.clone(), bound.epoch));
        let mut stale = bound.clone();
        stale.incarnation = "previous".into();
        assert!(store.bind_argv_mailbox_checked(&stale, &member, &|| Ok(())).is_err());
        let moved = declare_argv(&store, "other");
        assert!(store.bind_argv_mailbox_checked(&bound, &moved, &|| Ok(())).is_err());
        assert!(store.bind_argv_mailbox_checked(&bound, &member, &|| Ok(())).is_err());
        let mut undeclared = request();
        undeclared.subject = "agent/undeclared".into();
        assert!(store.bind_argv_mailbox_checked(&undeclared, &member, &|| Ok(())).is_err());
        store.append_claim(&ClaimInput {
            subject: bound.subject.clone(), kind: "runtime.observed".into(), actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!("replacement"))]),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        assert!(store.bind_argv_mailbox_checked(&bound, &member, &|| Ok(())).is_err());
    }

    #[test]
    fn argv_channel_cannot_discard_qualified_or_revoked_custody() {
        for revoked in [false, true] {
            let store = fixture();
            let owner = authority(1);
            let held = store.bind_mailbox_with_lease(&request(), Some(&owner)).unwrap();
            if revoked {
                store.connection.write().execute("UPDATE local_mailbox_leases SET revoked=1", []).unwrap();
            }
            let member = declare_argv(&store, "node");
            for candidate in [held.clone(), request()] {
                assert!(store.bind_argv_mailbox_checked(&candidate, &member, &|| Ok(())).is_err());
            }
            let retained = lease(&store.readers.get(), &held).unwrap().unwrap();
            assert_eq!((retained.0.token, retained.0.epoch, retained.1, retained.2),
                (held.token, held.epoch, owner, revoked));
        }
    }

    #[test]
    fn argv_capability_refuses_changed_declarations_before_receipt_commit() {
        for replacement in [
            "version 2\nstop \"agent/eval.worker\"",
            "version 2\nagent \"eval.worker\" { host \"other\"; workspace \"/tmp\"; argv \"python3\" \"probe\"; }",
            "version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; harness \"omp\" {} }",
            "version 2\nagent \"eval.worker\" { host \"node\"; workspace \"/tmp\"; argv \"python3\" \"different-program\"; }",
        ] {
            let store = fixture();
            let member = declare_argv(&store, "node");
            let bound = store.bind_argv_mailbox_checked(&request(), &member, &|| Ok(())).unwrap();
            let runtime = store.latest_claim(&bound.subject, Some("runtime.observed")).unwrap().unwrap().id;
            store.append_claim(&ClaimInput {
                subject: "message/argv-fence".into(), kind: "message.sent".into(), actor: Some("person/fixture".into()),
                fields: BTreeMap::from([("status".into(), json!("sent")), ("from".into(), json!("person/fixture")),
                    ("to".into(), json!(bound.subject)), ("content".into(), json!("Original envelope"))]),
                evidence: vec![], expected_subject: None, idempotency_key: None,
            }).unwrap();
            let receipt = |phase: &str| ClaimInput {
                subject: "message/argv-fence".into(), kind: format!("message.{phase}"), actor: Some(bound.subject.clone()),
                fields: BTreeMap::from([("status".into(), json!(phase))]), evidence: vec![], expected_subject: None,
                idempotency_key: Some(format!("argv-fence:{phase}")),
            };
            // Positive unchanged argv, including a successful receipt and the
            // pre-I/O admission that a caller may already have observed.
            store.append_mailbox_receipt_outcome(&receipt("staged"), &bound).unwrap();
            store.check_mailbox(&bound).unwrap();
            let changed = crate::graph::parse_intent(replacement, "node").unwrap();
            store.apply_internal(&changed, "change-argv-without-runtime-reconciliation").unwrap();
            assert_eq!(store.latest_claim(&bound.subject, Some("runtime.observed")).unwrap().unwrap().id, runtime);
            let held: (String, u64) = store.readers.get().query_row(
                "SELECT incarnation,epoch FROM local_mailbox_owners WHERE subject=?1 AND component=?2",
                params![bound.subject,bound.component], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            assert_eq!(held, (bound.incarnation.clone(),bound.epoch));
            assert_eq!(store.check_mailbox(&bound).unwrap_err().code, "stale-mailbox-session");
            for phase in ["staged","delivered","read"] {
                // Even a cached original receipt must be fenced before its retry;
                // fresh lifecycle claims are checked under the commit writer.
                assert_eq!(store.append_mailbox_receipt_outcome(&receipt(phase), &bound).unwrap_err().code, "stale-mailbox-session");
            }
            assert!(store.claims_for("message/argv-fence",Some("message.delivered")).unwrap().is_empty());
            assert!(store.claims_for("message/argv-fence",Some("message.read")).unwrap().is_empty());
            assert!(store.mailbox_lease_authority(&bound).unwrap().is_none());
        }
    }

    #[test]
    fn argv_designation_survives_reopen_and_cannot_become_native_custody() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("argv.db");
        let store = Store::open(&path,"node").unwrap();
        declare(&store);
        crate::mailbox::tests::ready(&store,"current");
        let member = declare_argv(&store,"node");
        let bound = store.bind_argv_mailbox_checked(&request(), &member, &|| Ok(())).unwrap();
        drop(store);
        let store = Store::open(&path,"node").unwrap();
        store.check_mailbox(&bound).unwrap();
        declare(&store);
        assert!(store.check_mailbox(&bound).is_err());
        assert!(store.bind_mailbox_with_lease(&bound,Some(&authority(1))).is_err());
        assert!(store.mailbox_lease_authority(&bound).unwrap().is_none());
        let member = declare_argv(&store,"node");
        store.connection.write().execute("DELETE FROM local_mailbox_bindings WHERE token=?1",[&bound.token]).unwrap();
        let mut retired = bound.clone(); retired.epoch=0;
        assert!(store.bind_argv_mailbox_checked(&retired,&member,&|| Ok(())).is_err());
        assert_eq!(store.readers.get().query_row("SELECT epoch FROM local_mailbox_owners WHERE subject=?1 AND component=?2",
            params![bound.subject,bound.component],|row|row.get::<_,u64>(0)).unwrap(),bound.epoch);
    }

    #[test]
    fn same_process_session_reexec_advances_ownership_without_reissuing_capability() {
        let store = fixture();
        let owner = authority(1);
        let bound = store.bind_mailbox_with_lease(&request(), Some(&owner)).unwrap();
        let mut advanced = owner.clone();
        advanced.sequence += 1;
        for foreign in [
            Authority { pid: owner.pid + 1, ..advanced.clone() },
            Authority { process_token: owner.process_token + 1, ..advanced.clone() },
            Authority { provider: "codex".into(), ..advanced.clone() },
        ] {
            assert!(store.promote_current_mailbox_ownership(&bound, &foreign).is_err());
            assert_eq!(store.mailbox_lease_authority(&bound).unwrap(), Some(owner.clone()));
        }
        let different_session = Authority { session: "different-session".into(), ..advanced.clone() };
        store.promote_current_mailbox_ownership(&bound, &different_session).unwrap();
        assert!(!store.owns_mailbox_lease(&bound, &different_session).unwrap());
        assert_eq!(store.mailbox_lease_authority(&bound).unwrap(), Some(owner.clone()));
        store.promote_current_mailbox_ownership(&bound, &advanced).unwrap();
        assert_eq!(store.mailbox_lease_authority(&bound).unwrap(), Some(advanced.clone()));
        let reconnected = store.bind_mailbox_with_lease(&bound, Some(&advanced)).unwrap();
        assert_eq!(
            (reconnected.token, reconnected.epoch, reconnected.incarnation),
            (bound.token.clone(), bound.epoch, bound.incarnation.clone()),
        );
        assert!(store.bind_mailbox_with_lease(&request(), Some(&advanced)).is_err());
        store.promote_current_mailbox_ownership(&bound, &owner).unwrap();
        assert_eq!(store.mailbox_lease_authority(&bound).unwrap(), Some(advanced));
        let successor = authority(3);
        let replacement = store.bind_mailbox_with_lease(&request(), Some(&successor)).unwrap();
        store.promote_current_mailbox_ownership(&bound, &successor).unwrap();
        assert_eq!(store.mailbox_lease_authority(&replacement).unwrap(), Some(successor));
        assert!(store.check_mailbox(&bound).is_err());
    }

    #[test]
    fn delivery_and_title_have_independent_custody_and_provider_mismatch_is_terminal() {
        let store = fixture();
        let owner = authority(1);
        let delivery = store
            .bind_mailbox_with_lease(&request(), Some(&owner))
            .unwrap();
        let title_request = Fence::new("agent/eval.worker", "current", "title");
        let title = store
            .bind_mailbox_with_lease(&title_request, Some(&owner))
            .unwrap();
        assert_eq!(title.epoch, 1);
        store.check_mailbox(&delivery).unwrap();
        store.check_mailbox(&title).unwrap();
        let mut wrong = authority(2);
        wrong.provider = "claude".into();
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&wrong))
                .is_err()
        );
        let mut wrong_runtime = request();
        wrong_runtime.incarnation = "retired".into();
        assert!(
            store
                .bind_mailbox_with_lease(&wrong_runtime, Some(&owner))
                .is_err()
        );
        store.check_mailbox(&delivery).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE local_mailbox_leases SET revoked=1 WHERE component='delivery'",
                [],
            )
            .unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&delivery, Some(&owner))
                .is_err()
        );
        assert!(store.repair_mailbox(&delivery, &owner).is_err());
        store.check_mailbox(&title).unwrap();
    }

    #[test]
    fn intentional_stop_cannot_become_automatic_repair_or_new_admission() {
        let store = fixture();
        let owner = authority(1);
        let fence = store
            .bind_mailbox_with_lease(&request(), Some(&owner))
            .unwrap();
        let stopped =
            crate::graph::parse_intent("version 2\nstop \"agent/eval.worker\"", "node").unwrap();
        store
            .apply_internal(&stopped, "intentional-stop-fixture")
            .unwrap();
        assert!(
            store.check_mailbox(&fence).is_err(),
            "a live wrapper must not read after intentional stop"
        );
        assert!(store.mailbox_session_active(&fence, &owner).is_err());
        assert!(store.repair_mailbox(&fence, &owner).is_err());
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(2)))
                .is_err()
        );
    }

    #[test]
    fn empty_registry_adopts_only_the_current_legacy_capability_without_epoch_churn() {
        let store = fixture();
        let old = store.bind_mailbox(&request()).unwrap();
        let current = store.bind_mailbox(&request()).unwrap();
        assert!(
            store
                .bind_mailbox_with_lease(&request(), Some(&authority(2)))
                .is_err()
        );
        assert!(
            store
                .bind_mailbox_with_lease(&old, Some(&authority(2)))
                .is_err()
        );
        let adopted = store
            .bind_mailbox_with_lease(&current, Some(&authority(2)))
            .unwrap();
        assert_eq!(
            (adopted.token, adopted.epoch),
            (current.token.clone(), current.epoch)
        );
        assert_eq!(
            store.mailbox_lease_authority(&current).unwrap(),
            Some(authority(2))
        );
        assert!(store.repair_mailbox(&old, &authority(1)).is_err());
        store.check_mailbox(&current).unwrap();
    }
}

fn refused(reason: &str) -> St3Error {
    St3Error::new("stale-mailbox-session", reason)
}

/// Durable designation of a physically admitted argv capability. It is not a
/// native provider lease and cannot be inferred from the current declaration.
fn argv_binding(connection: &Connection, fence: &Fence) -> Result<Option<MemberSpec>, St3Error> {
    let held: Option<(Fence, String)> = connection.prepare_cached(
        "SELECT subject,component,incarnation,epoch,member FROM local_mailbox_argv_bindings WHERE token=?1",
    ).map_err(internal)?.query_row([&fence.token], |row| Ok((Fence {
        token: fence.token.clone(), subject: row.get(0)?, component: row.get(1)?,
        incarnation: row.get(2)?, epoch: row.get(3)?,
    }, row.get(4)?))).optional().map_err(internal)?;
    let Some((held, member)) = held else { return Ok(None); };
    if held.subject != fence.subject || held.component != fence.component
        || held.incarnation != fence.incarnation || held.epoch != fence.epoch
    {
        return Err(refused("the argv capability belongs to another binding"));
    }
    serde_json::from_str(&member).map(Some).map_err(internal)
}

pub(super) fn refuse_argv_native_adoption(connection: &Connection, fence: &Fence) -> Result<(), St3Error> {
    let retained: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_mailbox_argv_bindings WHERE token=?1)",
        [&fence.token], |row| row.get(0),
    ).map_err(internal)?;
    if retained { return Err(refused("an argv capability cannot be reissued or adopted as native custody")); }
    Ok(())
}

pub(super) fn record_argv_binding(connection: &Connection, fence: &Fence, member: &MemberSpec) -> Result<(), St3Error> {
    connection.execute(
        "INSERT INTO local_mailbox_argv_bindings VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(token) DO NOTHING",
        params![fence.token,fence.subject,fence.component,fence.incarnation,fence.epoch,serde_json::to_string(member).map_err(internal)?],
    ).map_err(internal)?;
    Ok(())
}

pub(super) fn check_lease_fence(
    connection: &Connection,
    fence: &Fence,
    host: &str,
) -> Result<(), St3Error> {
    if let Some(member) = argv_binding(connection, fence)? {
        check_argv_declaration(connection, fence, &member, host)?;
    }
    let Some((held, owner, revoked)) = lease(connection, fence)? else {
        return Ok(());
    };
    if revoked
        || held.token != fence.token
        || held.epoch != fence.epoch
        || held.incarnation != fence.incarnation
    {
        return Err(refused("the authenticated lease was superseded or revoked"));
    }
    let desired = current_desired_row(connection, &fence.subject)
        .map_err(internal)?
        .ok_or_else(|| refused("the seat is no longer declared"))?;
    let member: Option<MemberSpec> = desired
        .member
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(internal)?;
    if desired.kind == "stop"
        || member.as_ref().is_none_or(|member| {
            member.driver.as_deref() != Some(&owner.provider)
                || member.host != host
                || member
                    .terminal_binding
                    .as_ref()
                    .is_some_and(|binding| binding.agent_incarnation() != fence.incarnation)
        })
    {
        return Err(refused(
            "the lease's declared provider was stopped or replaced",
        ));
    }
    Ok(())
}

pub(super) fn check_declaration(
    connection: &Connection,
    fence: &Fence,
    authority: &Authority,
    host: &str,
) -> Result<(), St3Error> {
    let declared = current_desired_row(connection, &fence.subject)
        .map_err(internal)?
        .ok_or_else(|| refused("the seat is no longer declared"))?;
    let member: MemberSpec = declared
        .member
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(internal)?
        .ok_or_else(|| refused("the seat is intentionally stopped"))?;
    if declared.kind == "stop"
        || member.driver.as_deref() != Some(&authority.provider)
        || member.host != host
        || member
            .terminal_binding
            .as_ref()
            .is_some_and(|binding| binding.agent_incarnation() != fence.incarnation)
    {
        return Err(refused(
            "the seat is stopped, moved or belongs to another provider",
        ));
    }
    Ok(())
}

pub(super) fn check_argv_declaration(
    connection: &Connection,
    fence: &Fence,
    captured: &MemberSpec,
    host: &str,
) -> Result<(), St3Error> {
    if lease(connection, fence)?.is_some() {
        return Err(refused("qualified mailbox history cannot become an argv-only transport"));
    }
    let desired = current_desired_row(connection, &fence.subject)
        .map_err(internal)?
        .ok_or_else(|| refused("the argv seat is no longer declared"))?;
    let current: MemberSpec = desired.member.as_deref()
        .map(serde_json::from_str).transpose().map_err(internal)?
        .ok_or_else(|| refused("the argv seat is intentionally stopped"))?;
    if desired.kind == "stop"
        || current.kind != MemberKind::Agent
        || current.host != host
        || current.driver.is_some()
        || current.terminal_binding.is_some()
        || !matches!(&current.launch, LaunchSpec::Argv(argv) if !argv.is_empty())
        || serde_json::to_value(&current).map_err(internal)?
            != serde_json::to_value(captured).map_err(internal)?
    {
        return Err(refused("the explicit argv seat declaration was stopped or replaced"));
    }
    Ok(())
}

pub(super) fn admit(
    connection: &Connection,
    request: &Fence,
    authority: &Authority,
) -> Result<(), St3Error> {
    admit_with_probe(connection, request, authority, |owner| {
        channel_liveness(owner, st_runtime::process_start_token(owner.pid), || {
            (unsafe { libc::kill(owner.pid as i32, 0) }) != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        })
    })
}

fn channel_liveness(
    owner: &Authority,
    birth: anyhow::Result<u64>,
    absent: impl FnOnce() -> bool,
) -> ChannelLiveness {
    match birth {
        Ok(token) if token == owner.process_token => ChannelLiveness::Alive,
        Ok(_) => ChannelLiveness::Dead,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                && absent() =>
        {
            ChannelLiveness::Dead
        }
        Err(_) => ChannelLiveness::Indeterminate,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelLiveness {
    Alive,
    Dead,
    Indeterminate,
}

fn admit_with_probe(
    connection: &Connection,
    request: &Fence,
    authority: &Authority,
    liveness: impl FnOnce(&Authority) -> ChannelLiveness,
) -> Result<(), St3Error> {
    if authority.session.is_empty()
        || (authority.sequence == 0
            && (authority.provider != "codex"
                || request.component != "delivery"
                || authority.session != format!("runtime:{}", request.incarnation)))
        || authority.sequence > i64::MAX as u64
    {
        return Err(refused("missing authenticated provider ownership"));
    }
    let Some((owner, prior, revoked)) = lease(connection, request)? else {
        // A restarted registry is not takeover permission. A pre-lease current binding
        // may be authenticated in place, but opaque same-runtime history cannot grant a
        // fresh token seniority over its current owner.
        if request.epoch == 0 {
            let unqualified_owner: Option<String> = connection.query_row(
                "SELECT incarnation FROM local_mailbox_owners WHERE subject=?1 AND component=?2",
                params![request.subject, request.component], |row| row.get(0),
            ).optional().map_err(internal)?;
            if unqualified_owner.as_deref() == Some(&request.incarnation) {
                let current_token: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_mailbox_bindings binding JOIN local_mailbox_owners owner
                     ON owner.subject=binding.subject AND owner.component=binding.component AND owner.incarnation=binding.incarnation AND owner.epoch=binding.epoch
                     WHERE binding.token=?1 AND binding.subject=?2 AND binding.component=?3 AND binding.incarnation=?4)",
                    params![request.token,request.subject,request.component,request.incarnation], |row| row.get(0),
                ).map_err(internal)?;
                if !current_token {
                    return Err(refused(
                        "unqualified ownership history does not authorize a fresh takeover",
                    ));
                }
            }
        }
        return Ok(());
    };
    if owner.incarnation != request.incarnation {
        return Ok(());
    }
    if revoked {
        return Err(refused("the mailbox lease was revoked"));
    }
    let bootstrap_promotion = prior.sequence == 0
        && authority.sequence > 0
        && prior.provider == authority.provider
        && prior.pid == authority.pid
        && prior.process_token == authority.process_token;
    if request.token == owner.token && (*authority == prior || bootstrap_promotion) {
        let bound: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM local_mailbox_bindings WHERE token=?1)",
                [&request.token],
                |row| row.get(0),
            )
            .map_err(internal)?;
        return if bound {
            Ok(())
        } else {
            Err(refused("the binding token was revoked"))
        };
    }
    // Only a captured newer provider record, or a provably dead channel process, replaces
    // custody. A duplicate attachment in the same session cannot silence the live channel.
    if authority.provider != prior.provider
        || authority.sequence < prior.sequence
        || (authority.sequence == prior.sequence
            && (authority.session != prior.session || liveness(&prior) != ChannelLiveness::Dead))
    {
        return Err(refused(
            "another live channel holds this provider session's lease",
        ));
    }
    // Old capabilities remain terminal even after the old process disappears.
    if request.epoch != 0 {
        return Err(refused(
            "a retired binding cannot acquire a successor lease",
        ));
    }
    Ok(())
}

pub(super) fn record(
    connection: &Connection,
    fence: &Fence,
    authority: &Authority,
) -> Result<(), St3Error> {
    connection
        .execute(
            "INSERT INTO local_mailbox_leases VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0)
         ON CONFLICT(subject,component) DO UPDATE SET incarnation=excluded.incarnation,
         provider=excluded.provider,session=excluded.session,sequence=excluded.sequence,
         pid=excluded.pid,process_token=excluded.process_token,token=excluded.token,
         epoch=excluded.epoch,revoked=0",
            params![
                fence.subject,
                fence.component,
                fence.incarnation,
                authority.provider,
                authority.session,
                authority.sequence,
                authority.pid,
                authority.process_token.to_string(),
                fence.token,
                fence.epoch
            ],
        )
        .map_err(internal)?;
    Ok(())
}

impl Store {
    pub(crate) fn mailbox_failure_episode(
        &self,
        fence: &Fence,
        episode: &str,
    ) -> Result<Option<(String, bool)>, St3Error> {
        let connection = self.readers.get();
        let latest: Option<String> = connection
            .prepare_cached(&canonical_sql(
                "SELECT id FROM claims WHERE subject=?1 AND kind='operational.failure'
             AND json_extract(body,'$.fields.episode')=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1",
            ))
            .map_err(internal)?
            .query_row(params![fence.subject, episode], |row| row.get(0))
            .optional()
            .map_err(internal)?;
        let Some(failure) = latest else {
            return Ok(None);
        };
        let recovered: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND kind='operational.recovered'
             AND json_extract(body,'$.fields.failure')=?2)", params![fence.subject, failure], |row| row.get(0),
        ).map_err(internal)?;
        Ok(Some((failure, !recovered)))
    }

    /// The caller holds the provider ownership lock. Recheck physical/declaration and
    /// canonical custody inside the same Store writer transaction as fault publication.
    pub(crate) fn append_mailbox_lease_claim(
        &self,
        fence: &Fence,
        owner: &Authority,
        input: &ClaimInput,
        repaired: bool,
    ) -> Result<ClaimRecord, St3Error> {
        let admission = |connection: &Connection| {
            check_mailbox_incarnation(connection, fence)?;
            check_declaration(connection, fence, owner, &self.origin)?;
            let Some((held, authority, revoked)) = lease(connection, fence)? else {
                return Err(refused("fault publication lost canonical custody"));
            };
            if revoked
                || held.token != fence.token
                || held.epoch != fence.epoch
                || held.incarnation != fence.incarnation
                || authority != *owner
            {
                return Err(refused("fault publication belongs to a retired lease"));
            }
            if repaired {
                check_mailbox_fence(connection, fence, &self.origin)?;
            }
            if let Some(revision) = input.fields.get("source_revision").and_then(Value::as_str)
                && current_desired_row(connection, &fence.subject)
                    .map_err(internal)?
                    .is_none_or(|row| row.claim_id != revision)
            {
                return Err(refused("the fault's configured owner declaration changed"));
            }
            Ok(())
        };
        append_claim_with_commit_context(&self.graph, input, None, None, None, None,
            ClaimCommitContext { admission: Some(&admission), ..Default::default() })
            .map(|(claim, _)| claim)
    }
    /// Promote physical Codex bootstrap custody, or advance a surviving process's
    /// same-session ownership after re-exec, under the caller's provider ownership lock.
    /// Both retain only the still-current capability and exact process generation.
    pub(crate) fn promote_current_mailbox_ownership(
        &self,
        fence: &Fence,
        authority: &Authority,
    ) -> Result<(), St3Error> {
        let mut connection = self.connection.write();
        let tx = connection.transaction().map_err(internal)?;
        let Some((held, prior, revoked)) = lease(&tx, fence)? else {
            return Ok(());
        };
        // Re-exec can claim a newer sequence for the surviving provider session.
        // Only that exact process and current capability may retain its lease.
        let advancing_session = prior.sequence > 0
            && authority.sequence > prior.sequence
            && authority.session == prior.session;
        if authority.sequence == 0 || (prior.sequence != 0 && !advancing_session) {
            return Ok(());
        }
        if held.incarnation != fence.incarnation
            || held.token != fence.token
            || held.epoch != fence.epoch
        {
            return Ok(());
        }
        if revoked
            || prior.pid != authority.pid
            || prior.process_token != authority.process_token
            || prior.provider != authority.provider
        {
            return Err(refused(
                "current custody belongs to another physical process",
            ));
        }
        check_mailbox_incarnation(&tx, fence)?;
        check_mailbox_fence(&tx, fence, &self.origin)?;
        check_declaration(&tx, fence, authority, &self.origin)?;
        record(&tx, fence, authority)?;
        tx.commit().map_err(internal)
    }
    pub(crate) fn mailbox_lease_authority(
        &self,
        fence: &Fence,
    ) -> Result<Option<Authority>, St3Error> {
        Ok(
            lease(&self.readers.get(), fence)?.and_then(|(held, owner, _)| {
                (held.incarnation == fence.incarnation
                    && held.token == fence.token
                    && held.epoch == fence.epoch)
                    .then_some(owner)
            }),
        )
    }

    /// A transient physical query may retry only a still-current canonical capability.
    /// An absent owner row can be repaired; a foreign owner or retired binding is terminal.
    pub(crate) fn check_mailbox_custody_fence(&self, fence: &Fence) -> Result<(), St3Error> {
        let connection = self.readers.get();
        check_mailbox_incarnation(&connection, fence)?;
        if lease(&connection, fence)?.is_none() {
            return check_mailbox_fence(&connection, fence, &self.origin);
        }
        check_lease_fence(&connection, fence, &self.origin)?;
        let bound: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_mailbox_bindings WHERE token=?1 AND subject=?2
             AND component=?3 AND incarnation=?4 AND epoch=?5)",
            params![fence.token, fence.subject, fence.component, fence.incarnation, fence.epoch],
            |row| row.get(0),
        ).map_err(internal)?;
        if !bound { return Err(refused("the binding token was revoked")); }
        let has_owner: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_mailbox_owners WHERE subject=?1 AND component=?2)",
            params![fence.subject,fence.component], |row| row.get(0),
        ).map_err(internal)?;
        if has_owner { check_mailbox_fence(&connection, fence, &self.origin)?; }
        Ok(())
    }

    pub(crate) fn mailbox_session_active(
        &self,
        fence: &Fence,
        authority: &Authority,
    ) -> Result<(), St3Error> {
        let connection = self.readers.get();
        check_mailbox_incarnation(&connection, fence)?;
        check_declaration(&connection, fence, authority, &self.origin)
    }

    pub(crate) fn has_mailbox_lease(&self, fence: &Fence) -> Result<bool, St3Error> {
        Ok(lease(&self.readers.get(), fence)?.is_some())
    }

    /// Captured public-card fault source, bounded to the selected subjects and cut.
    pub(crate) fn mailbox_faults_for(
        &self,
        subjects: &[String],
        index: u64,
    ) -> Result<BTreeMap<String, String>> {
        if subjects.is_empty() { return Ok(BTreeMap::new()); }
        // One captured statement, rather than up to three round trips per actor. This
        // local query bound also applies to non-HTTP captures and never extends a parent.
        let duration = std::time::Duration::from_millis(25);
        let budget = smallclaims::read_budget::current().map_or_else(
            || smallclaims::read_budget::ReadBudget::new("mailbox/card-faults", duration),
            |parent| parent.child(duration),
        );
        let checked = budget.clone();
        smallclaims::read_budget::with(Some(budget), || {
            checked.check()?;
            let read = || -> Result<BTreeMap<String, String>> {
            let connection = self.readers.get();
            let query = canonical_sql(
                "WITH selected(subject) AS (SELECT DISTINCT value FROM json_each(?1)),
                 failures AS MATERIALIZED (
                   SELECT subject, (SELECT id FROM claims INDEXED BY claims_subject_kind_accepted_index
                     WHERE claims.subject=selected.subject AND kind='operational.failure'
                     AND +store_index<=?2
                     AND json_extract(body,'$.fields.condition')='mailbox-channel-lost'
                     ORDER BY CANONICAL_DESC(claims) LIMIT 1) AS failure FROM selected
                 ), unresolved AS MATERIALIZED (
                   SELECT subject,failure FROM failures WHERE failure IS NOT NULL
                   AND NOT EXISTS(SELECT 1 FROM claims INDEXED BY claims_subject_kind_index
                     WHERE claims.subject=failures.subject AND kind='operational.recovered'
                     AND store_index<=?2 AND json_extract(body,'$.fields.failure')=failures.failure)
                 )
                 SELECT unresolved.subject, fault.body,
                   (SELECT body FROM claims INDEXED BY claims_subject_kind_accepted_index
                    WHERE claims.subject=unresolved.subject AND kind='runtime.observed'
                    AND +store_index<=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1)
                 FROM unresolved JOIN claims fault ON fault.id=unresolved.failure",
            );
            let mut statement = connection.prepare_cached(&query)?;
            let rows = statement.query_map(params![serde_json::to_string(subjects)?, index], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
            })?;
            let mut faults = BTreeMap::new();
            for row in rows {
                smallclaims::read_budget::check()?;
                let (subject, fault, runtime) = row?;
                let body: Value = serde_json::from_str(&fault)?;
                let runtime: Value = runtime.map(|body| serde_json::from_str(&body))
                    .transpose()?.unwrap_or(Value::Null);
                let fields = runtime.get("fields").unwrap_or(&runtime);
                // A fault's exact runtime must still be the running one at this same cut.
                if fields["status"] == "running"
                    && fields["incarnation_id"] == body["fields"]["incarnation"]
                {
                    faults.insert(subject, body["fields"]["reason"].as_str()
                        .unwrap_or("mailbox channel lost").into());
                }
            }
            smallclaims::read_budget::check()?;
            Ok(faults)
            };
            // Keep the authoritative existing snapshot; otherwise request_read acquires
            // through try_get, which fails closed instead of waiting for a returned reader.
            let pinned = smallclaims::sqlite::PINNED_READER.with(|slot|
                slot.borrow().as_ref().is_some_and(|(key, _)| *key == self.readers.key()));
            let result = if pinned { read() } else {
                self.readers.request_read(read).map_err(anyhow::Error::from).and_then(|result| result)
            };
            #[cfg(test)]
            AFTER_FAULT_ACQUISITION.with(|pause| {
                if let Some(pause) = pause.borrow_mut().take() { pause(); }
            });
            // Preserve the typed retryable deadline after restoring the parent scope,
            // including SQLite interruption before the first row could be decoded.
            checked.check()?;
            result
        })
    }

    pub(crate) fn owns_mailbox_lease(
        &self,
        fence: &Fence,
        authority: &Authority,
    ) -> Result<bool, St3Error> {
        Ok(
            lease(&self.readers.get(), fence)?.is_some_and(|(held, owner, _)| {
                held.incarnation == fence.incarnation
                    && held.token == fence.token
                    && held.epoch == fence.epoch
                    && owner == *authority
            }),
        )
    }

    /// Repair only the exact canonical lease. Never mint a token or advance an epoch.
    /// Deleting/revoking a binding is terminal; only displacement of its owner is repairable.
    #[cfg(test)]
    pub(crate) fn repair_mailbox(
        &self,
        fence: &Fence,
        authority: &Authority,
    ) -> Result<bool, St3Error> {
        self.repair_mailbox_checked(fence, authority, &|| Ok(()))
    }

    pub(crate) fn repair_mailbox_checked(
        &self,
        fence: &Fence,
        authority: &Authority,
        validate: &dyn Fn() -> Result<(), St3Error>,
    ) -> Result<bool, St3Error> {
        let mut connection = self.connection.write();
        let tx = connection.transaction().map_err(internal)?;
        check_mailbox_incarnation(&tx, fence)?;
        validate()?;
        check_declaration(&tx, fence, authority, &self.origin)?;
        let Some((owner, held, revoked)) = lease(&tx, fence)? else {
            return Err(refused("this binding has no authenticated recovery lease"));
        };
        if revoked
            || owner.token != fence.token
            || owner.epoch != fence.epoch
            || owner.incarnation != fence.incarnation
            || held != *authority
        {
            return Err(refused(
                "the authenticated mailbox lease is superseded or revoked",
            ));
        }
        let binding: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM local_mailbox_bindings WHERE token=?1 AND
             subject=?2 AND component=?3 AND incarnation=?4 AND epoch=?5)",
                params![
                    fence.token,
                    fence.subject,
                    fence.component,
                    fence.incarnation,
                    fence.epoch
                ],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if !binding {
            return Err(refused("the binding token was revoked"));
        }
        if check_mailbox_fence(&tx, fence, &self.origin).is_ok() {
            return Ok(false);
        }
        let owner_epoch: Option<u64> = tx
            .query_row(
                "SELECT epoch FROM local_mailbox_owners WHERE subject=?1 AND component=?2",
                params![fence.subject, fence.component],
                |row| row.get(0),
            )
            .optional()
            .map_err(internal)?;
        if owner_epoch.is_some() {
            return Err(refused("a successor owner cannot be rewound by recovery"));
        }
        tx.execute(
            "INSERT INTO local_mailbox_owners VALUES (?1,?2,?3,?4)
             ON CONFLICT(subject,component) DO UPDATE SET incarnation=excluded.incarnation,epoch=excluded.epoch",
            params![fence.subject,fence.component,fence.incarnation,fence.epoch],
        ).map_err(internal)?;
        check_mailbox_fence(&tx, fence, &self.origin)?;
        tx.commit().map_err(internal)?;
        if let Some(wakes) = self.smalltalk.mailbox_wakes.get() {
            wakes.owner_changed(&fence.subject, &fence.component);
        }
        Ok(true)
    }
}

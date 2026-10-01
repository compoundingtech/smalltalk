//! Tests of `smallclaims::store::checkpoint_agreement` through smalltalk's store.
use super::*;
use smallclaims::store::checkpoint_agreement::*;

const FLEET: &str = "fleet/test";
const CUT: u128 = 20 * DAY_MS;

fn claim(kind: &str, subject: &str, writer: &str, fields: Value) -> CheckpointClaim {
    CheckpointClaim {
        id: format!("{kind}:{subject}:{writer}:{fields}"),
        kind: kind.into(),
        subject: subject.into(),
        writer: writer.into(),
        actor: None,
        fields: fields.as_object().cloned().unwrap_or_default(),
    }
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn seal(writer: &str, participants: &[&str], digest: &str) -> CheckpointClaim {
    claim(
        CHECKPOINT_SEALED,
        &checkpoint_name(CUT),
        writer,
        json!({
            "cut_unix_ms": CUT, "participants": participants, "sealed_digest": digest,
            "sealed_count": 1, "rules_digest": "rules", "checkpoint_protocol": 1,
        }),
    )
}

fn verified(writer: &str, participants: &[&str], drop: &str) -> CheckpointClaim {
    claim(
        CHECKPOINT_VERIFIED,
        &checkpoint_name(CUT),
        writer,
        json!({
            "cut_unix_ms": CUT, "participants": participants, "sealed_digest": "sealed",
            "rules_digest": "rules", "drop_digest": drop, "dropped_envelopes": 1,
            "dropped_claims": 1, "retained_digest": "retained", "graph_digest": "graph",
            "reader_digest": "reader", "checkpoint_protocol": 1,
        }),
    )
}

fn excusal(writer: &str, actor: &str) -> CheckpointClaim {
    CheckpointClaim {
        actor: Some(actor.into()),
        ..claim(
            CHECKPOINT_EXCUSED,
            &format!("checkpoint-excusal/{writer}"),
            "alder",
            json!({"writer": writer, "reason": "gone"}),
        )
    }
}

#[test]
fn participants_are_known_writers_less_leavers_and_excused_writers() {
    let known = names(&["alder", "birch", "cedar", "dogwood"]);
    let left = names(&["dogwood"]);
    // A writer named only by another node's seal is a participant here too.
    let claims = vec![seal("alder", &["alder", "birch", "cedar", "elm"], "d")];
    assert_eq!(
        participants(&known, &left, &claims),
        names(&["alder", "birch", "cedar", "elm"])
    );

    // A person's excusal takes a writer out; an agent's or the system's does nothing.
    let mut claims = claims;
    claims.push(excusal("cedar", "person/operator"));
    claims.push(excusal("birch", "agent/alder.worker"));
    claims.push(CheckpointClaim {
        actor: None,
        ..excusal("elm", "person/operator")
    });
    assert_eq!(
        participants(&known, &left, &claims),
        names(&["alder", "birch", "elm"])
    );
    // The excused writer's own next seal ends its excusal.
    claims.push(seal("cedar", &["alder", "birch", "cedar"], "d"));
    assert_eq!(excused_writers(&claims), BTreeSet::new());
    assert!(participants(&known, &left, &claims).contains("cedar"));
    // A removal is not a leave: a removed writer stays until it is excused.
    assert!(participants(&known, &BTreeSet::new(), &claims).contains("dogwood"));
}

#[test]
fn a_certificate_needs_every_participant_with_identical_terms() {
    let all = ["alder", "birch", "cedar"];
    let complete = all.map(|writer| verified(writer, &all, "drop"));
    let checkpoint = checkpoint_name(CUT);
    assert_eq!(certificates(&complete, &checkpoint).len(), 1);
    let stable = stable_checkpoints(&complete);
    assert_eq!(stable.keys().copied().collect::<Vec<_>>(), [CUT]);

    // One participant missing.
    assert!(certificates(&complete[..2], &checkpoint).is_empty());
    // One digest different.
    let mut different = complete.to_vec();
    different[2] = verified("cedar", &all, "another drop");
    assert!(certificates(&different, &checkpoint).is_empty());
    // The participants disagree.
    let mut disagreeing = complete.to_vec();
    disagreeing[2] = verified("cedar", &["alder", "birch", "cedar", "dogwood"], "drop");
    assert!(certificates(&disagreeing, &checkpoint).is_empty());
    // A second verification from a node is ignored, before or after the certificate.
    let mut second = vec![complete[0].clone(), verified("alder", &all, "other")];
    second.extend_from_slice(&complete[1..]);
    assert_eq!(
        certificates(&second, &checkpoint),
        certificates(&complete, &checkpoint)
    );
    let mut late = complete.to_vec();
    late.push(verified("birch", &all, "other"));
    late.push(seal("alder", &all, "resealed"));
    assert_eq!(
        certificates(&late, &checkpoint),
        certificates(&complete, &checkpoint)
    );
    // A seal is not a verification.
    let seals = all.map(|writer| seal(writer, &all, "sealed"));
    assert!(certificates(&seals, &checkpoint).is_empty());
}

#[test]
fn stability_does_not_depend_on_claim_order() {
    let all = ["alder", "birch", "cedar"];
    let mut claims = all.map(|writer| verified(writer, &all, "drop")).to_vec();
    claims.extend(all.map(|writer| seal(writer, &all, "sealed")));
    claims.push(excusal("dogwood", "person/operator"));
    let expected = stable_checkpoints(&claims);
    for rotation in 0..claims.len() {
        let mut rotated = claims.clone();
        rotated.rotate_left(rotation);
        assert_eq!(stable_checkpoints(&rotated), expected);
        rotated.reverse();
        assert_eq!(stable_checkpoints(&rotated), expected);
    }
}

#[test]
fn both_sides_of_an_excused_partition_can_certify_and_a_node_adopts_both() {
    // People excused each side of a partition. Each side certified the same cut alone.
    let west = ["alder", "birch"];
    let east = ["cedar", "dogwood"];
    let mut claims = west.map(|writer| verified(writer, &west, "west")).to_vec();
    claims.extend(east.map(|writer| verified(writer, &east, "east")));
    let certified = certificates(&claims, &checkpoint_name(CUT));
    assert_eq!(certified.len(), 2);
    claims.reverse();
    assert_eq!(certificates(&claims, &checkpoint_name(CUT)), certified);
    // Every node applies the same one: here the smaller drop digest, since both sides
    // have two participants.
    let chosen = chosen_certificate(&certified).unwrap();
    assert_eq!(chosen.terms.drop_digest, "east");
    let mut reversed = certified.clone();
    reversed.reverse();
    assert_eq!(chosen_certificate(&reversed), Some(chosen));
    // A side with more participants wins whatever its digest.
    let larger = ["cedar", "dogwood", "elm"];
    let mut claims = west.map(|writer| verified(writer, &west, "west")).to_vec();
    claims.extend(larger.map(|writer| verified(writer, &larger, "zzz")));
    let certified = certificates(&claims, &checkpoint_name(CUT));
    assert_eq!(
        chosen_certificate(&certified).unwrap().terms.drop_digest,
        "zzz"
    );
    assert_eq!(chosen_certificate(&[]), None);
}

// Stores exchanging for real.

fn sync(nodes: &[&Store]) {
    for node in nodes {
        node.bind_fleet(FLEET).unwrap();
    }
    for _ in 0..3 {
        for source in nodes {
            for target in nodes {
                if source.origin == target.origin {
                    continue;
                }
                let exchange = source
                    .export_replication_exchange(
                        FLEET,
                        &target.replication_inventory().unwrap(),
                    )
                    .unwrap();
                target
                    .receive_replication_exchange(&source.origin, FLEET, &exchange)
                    .unwrap();
                target.validate_replication_backlog().unwrap();
                target.project_replication_backlog().unwrap();
            }
        }
    }
}

fn observe(store: &Store, n: usize) {
    for index in 0..n {
        store
            .append_claim(&ClaimInput {
                subject: format!("daemon/{}", store.origin),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("code".into(), json!("slow-request")),
                    ("reason".into(), json!(format!("slow {index}"))),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "{}-slow-{index}-{}",
                    store.origin,
                    Uuid::now_v7().simple()
                )),
            })
            .unwrap();
    }
}

/// A context two days after tomorrow's cut, so everything written now is before the cut.
fn context(scratch: &Path, days_later: u128) -> CheckpointContext {
    CheckpointContext {
        now_unix_ms: now_ms() + (3 + days_later) * DAY_MS,
        configured_peers: Vec::new(),
        scratch: scratch.to_path_buf(),
        reviewer: "person/operator".into(),
    }
}

fn step(store: &Store, context: &CheckpointContext) -> Vec<CheckpointAction> {
    store.checkpoint_step(context).unwrap()
}

fn kinds(actions: &[CheckpointAction]) -> Vec<&'static str> {
    actions
        .iter()
        .map(|action| match action {
            CheckpointAction::Sealed { .. } => "sealed",
            CheckpointAction::Verified { .. } => "verified",
            CheckpointAction::ProofFailed { .. } => "proof-failed",
            CheckpointAction::AttentionRequested { .. } => "attention",
            CheckpointAction::AttentionWithdrawn { .. } => "withdrawn",
            CheckpointAction::Trimmed { .. } => "trimmed",
            CheckpointAction::ManifestNeeded { .. } => "manifest-needed",
            CheckpointAction::TrimGraphChanged { .. } => "graph-changed",
        })
        .collect()
}

fn stable_cuts(store: &Store) -> Vec<u128> {
    stable_checkpoints(&store.checkpoint_claims().unwrap())
        .into_keys()
        .collect()
}

#[test]
fn nodes_seal_then_verify_once_every_participant_sealed_the_same_set() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let cut = newest_due_cut(context.now_unix_ms);
    let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
    observe(&alder, 4);
    observe(&birch, 3);
    sync(&[&alder, &birch]);

    assert_eq!(kinds(&step(&alder, &context)), ["sealed"]);
    // Nothing more until birch has sealed the same set.
    assert!(step(&alder, &context).is_empty());
    assert_eq!(kinds(&step(&birch, &context)), ["sealed"]);
    sync(&[&alder, &birch]);
    assert_eq!(kinds(&step(&alder, &context)), ["verified"]);
    assert!(stable_cuts(&alder).is_empty());
    assert_eq!(kinds(&step(&birch, &context)), ["verified"]);
    sync(&[&alder, &birch]);
    for node in [&alder, &birch] {
        assert_eq!(stable_cuts(node), [cut]);
        assert_eq!(kinds(&step(node, &context)), ["trimmed"]);
        assert!(step(node, &context).is_empty());
    }
    let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
    let stable = status.newest_stable.unwrap();
    assert_eq!(stable.terms.participants, names(&["alder", "birch"]));
    assert!(status.pending.is_none());
    // Seals and verifications are dated at or after the cut, even though the clock was not.
    for claim in alder
        .claims_page(None, None, 0, None, false, 10_000)
        .unwrap()
        .claims
    {
        if claim.kind.starts_with("checkpoint.") {
            assert!(claim.accepted_at_unix_ms >= cut, "{claim:?}");
        } else {
            assert!(claim.accepted_at_unix_ms < cut, "{claim:?}");
        }
    }
}

#[test]
fn a_late_envelope_before_the_cut_reseals_until_someone_verified() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let [alder, birch, cedar] =
        ["alder", "birch", "cedar"].map(|name| Store::open_memory(name).unwrap());
    observe(&alder, 2);
    observe(&birch, 2);
    sync(&[&alder, &birch]);
    step(&alder, &context);
    step(&birch, &context);

    // An envelope from before the cut reaches alder late, from a writer it had not heard
    // of. Alder seals again, now with cedar as a participant.
    observe(&cedar, 1);
    sync(&[&alder, &cedar]);
    let actions = step(&alder, &context);
    let CheckpointAction::Sealed { participants, .. } = &actions[0] else {
        panic!("{actions:?}");
    };
    assert_eq!(participants, &names(&["alder", "birch", "cedar"]));
    sync(&[&alder, &birch, &cedar]);
    for node in [&birch, &cedar] {
        assert_eq!(kinds(&step(node, &context)), ["sealed"]);
    }
    sync(&[&alder, &birch, &cedar]);
    assert_eq!(kinds(&step(&alder, &context)), ["verified"]);

    // Once alder verified, a later envelope before the cut never makes it seal again.
    let dogwood = Store::open_memory("dogwood").unwrap();
    observe(&dogwood, 1);
    sync(&[&alder, &dogwood]);
    assert!(step(&alder, &context).is_empty());
    let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
    assert!(status.pending.unwrap().unsealed.contains("dogwood"));
}

#[test]
fn a_silent_participant_holds_everything_up_until_a_person_excuses_it() {
    let scratch = tempfile::tempdir().unwrap();
    let first = context(scratch.path(), 0);
    let [alder, birch, cedar] =
        ["alder", "birch", "cedar"].map(|name| Store::open_memory(name).unwrap());
    observe(&alder, 2);
    observe(&birch, 2);
    // Cedar is an old build, or away: it wrote once and never seals.
    observe(&cedar, 1);
    sync(&[&alder, &birch, &cedar]);
    step(&alder, &first);
    step(&birch, &first);
    sync(&[&alder, &birch]);
    for _ in 0..3 {
        assert!(!kinds(&step(&alder, &first)).contains(&"verified"));
        assert!(!kinds(&step(&birch, &first)).contains(&"verified"));
    }

    // Three days after it became due, the first participant to have sealed asks a person.
    let late = CheckpointContext {
        now_unix_ms: first.now_unix_ms + CHECKPOINT_ATTENTION_AFTER_MS,
        ..first.clone()
    };
    // The newest due checkpoint moved on; seal it first.
    step(&alder, &late);
    step(&birch, &late);
    sync(&[&alder, &birch]);
    step(&alder, &late);
    let items = alder
        .checkpoint_attention_items(Some("person/operator"), late.now_unix_ms)
        .unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].title.contains("cedar"));
    assert_eq!(
        serde_json::to_value(&items).unwrap(),
        serde_json::to_value(
            birch
                .checkpoint_attention_items(Some("person/operator"), late.now_unix_ms)
                .unwrap()
        )
        .unwrap()
    );
    assert!(alder.attention_requests(None, true).unwrap().is_empty());

    // An agent cannot excuse it.
    assert!(
        alder
            .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                writer: "cedar".into(),
                reason: "away".into(),
                actor: "agent/alder.worker".into(),
            })
            .is_err()
    );
    alder
        .excuse_checkpoint_writer(&CheckpointExcuseRequest {
            writer: "cedar".into(),
            reason: "the laptop is in a drawer".into(),
            actor: "person/operator".into(),
        })
        .unwrap();
    sync(&[&alder, &birch]);
    for node in [&alder, &birch] {
        assert_eq!(kinds(&step(node, &late)), ["sealed"]);
    }
    sync(&[&alder, &birch]);
    for node in [&alder, &birch] {
        assert_eq!(kinds(&step(node, &late)), ["verified"]);
    }
    sync(&[&alder, &birch]);
    let cut = newest_due_cut(late.now_unix_ms);
    assert_eq!(stable_cuts(&alder), [cut]);
    assert_eq!(kinds(&step(&alder, &late)), ["trimmed"]);
    assert_eq!(kinds(&step(&birch, &late)), ["trimmed"]);

    // Cedar comes back. What it wrote while away replicates. It adopts the checkpoint it
    // missed, and its next seal ends its excusal, so the next checkpoint waits for it.
    observe(&cedar, 1);
    sync(&[&alder, &birch, &cedar]);
    let next = CheckpointContext {
        now_unix_ms: late.now_unix_ms + DAY_MS,
        ..late.clone()
    };
    assert_eq!(kinds(&step(&cedar, &next)), ["manifest-needed"]);
    let need = cedar.checkpoint_manifest_need().unwrap().unwrap();
    let manifest = alder
        .checkpoint_manifest(&need.checkpoint, need.cut_unix_ms)
        .unwrap();
    assert_eq!(
        kinds(&cedar.adopt_checkpoint(&manifest).unwrap()),
        ["trimmed"]
    );
    assert_eq!(claim_ids(&cedar), claim_ids(&alder));
    assert_eq!(kinds(&step(&cedar, &next)), ["sealed"]);
    sync(&[&alder, &birch, &cedar]);
    assert!(
        alder
            .checkpoint_status(next.now_unix_ms, &[])
            .unwrap()
            .excused
            .is_empty()
    );
    let actions = step(&alder, &next);
    let CheckpointAction::Sealed { participants, .. } = &actions[0] else {
        panic!("{actions:?}");
    };
    assert!(participants.contains("cedar"));
}

#[test]
fn a_seal_dates_every_later_write_at_or_after_its_cut() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let cut = newest_due_cut(context.now_unix_ms);
    let alder = Store::open_memory("alder").unwrap();
    observe(&alder, 1);
    step(&alder, &context);
    // The clock goes back three days.
    alder.set_write_clock_offset(-(3 * DAY_MS as i64)).unwrap();
    observe(&alder, 3);
    alder
        .put_document("doc/alder-notes", b"invented notes", &None, "alder-notes")
        .unwrap();
    let claims = alder
        .claims_page(None, None, 0, None, false, 10_000)
        .unwrap()
        .claims;
    let after_seal = claims
        .iter()
        .skip_while(|claim| claim.kind != CHECKPOINT_SEALED)
        .collect::<Vec<_>>();
    assert!(after_seal.len() > 3);
    for claim in after_seal {
        assert!(claim.accepted_at_unix_ms >= cut, "{claim:?}");
    }
    // A clock ahead of the cut is used as it is.
    alder.set_write_clock_offset(10 * DAY_MS as i64).unwrap();
    observe(&alder, 1);
    let newest = alder
        .claims_page(None, None, 0, None, false, 10_000)
        .unwrap()
        .claims;
    assert!(newest.last().unwrap().accepted_at_unix_ms > cut + 5 * DAY_MS);
}

/// Folds read a subject's claims in canonical order, which starts with the accepted time. A
/// writer whose clock steps back must still date each new claim at or after its last one,
/// or its new state would sort before its old state and every node would show the old.
#[test]
fn a_writer_never_dates_a_claim_before_its_own_newest() {
    let alder = Store::open_memory("alder").unwrap();
    alder.set_write_clock_offset(2 * DAY_MS as i64).unwrap();
    observe(&alder, 2);
    alder.set_write_clock_offset(0).unwrap();
    observe(&alder, 2);
    let claims = alder
        .claims_page(None, None, 0, None, false, 10_000)
        .unwrap()
        .claims;
    assert_eq!(claims.len(), 4);
    for pair in claims.windows(2) {
        assert!(pair[1].accepted_at_unix_ms >= pair[0].accepted_at_unix_ms);
    }
    let newest = alder
        .latest_claim(
            &format!("daemon/{}", alder.origin),
            Some("daemon.diagnostic"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(newest.id, claims.last().unwrap().id);
}

/// A trim that would change the graph stops, records why, and seals nothing more until a
/// person has looked. A trim that only bumps the graph generation compares the digests and
/// goes on.
#[test]
fn a_trim_that_would_change_the_graph_waits_for_a_person() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
    stable_pair(&alder, &birch, &context);
    alder.set_trim_fault(Some(TrimFault::TouchGraph));
    assert_eq!(kinds(&step(&alder, &context)), ["trimmed"]);

    let mut projection_tables = projection_digest::tables(&birch.readers.get()).unwrap();
    projection_tables.remove("claim_sources");
    projection_tables.remove("operations");
    let previous_operations = projection_digest::operation_rows();
    let previous_operations = birch
        .readers
        .get()
        .prepare(&previous_operations)
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    let held = claim_ids(&birch);
    birch.set_trim_fault(Some(TrimFault::ChangeGraph));
    assert_eq!(kinds(&step(&birch, &context)), ["graph-changed"]);
    let mut current_tables = projection_digest::tables(&birch.readers.get()).unwrap();
    current_tables.remove("claim_sources");
    current_tables.remove("operations");
    let current_operations = birch
        .readers
        .get()
        .prepare(&projection_digest::operation_rows())
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap();
    assert!(current_operations.is_superset(&previous_operations));
    // The new checkpoint fault, diagnostic claims and their operation rows change digests.
    // Earlier shared rows remain intact when the trim transaction is refused.
    assert_eq!(current_tables, projection_tables);
    assert!(claim_ids(&birch).is_superset(&held));
    let status = birch.checkpoint_status(context.now_unix_ms, &[]).unwrap();
    assert!(status.halted);
    assert_eq!(status.trimmed, None);
    assert!(
        birch
            .claims_for("daemon/birch", Some("daemon.diagnostic"))
            .unwrap()
            .iter()
            .any(|claim| claim.body["fields"]["code"] == "checkpoint-trim-graph-changed")
    );

    // Nothing more happens, even when the next checkpoint is due.
    let next = CheckpointContext {
        now_unix_ms: context.now_unix_ms + DAY_MS,
        ..context.clone()
    };
    observe(&birch, 1);
    assert!(step(&birch, &next).is_empty());
    assert!(
        birch
            .resume_checkpoints("agent/birch.worker", "looked")
            .is_err()
    );
    assert!(birch.resume_checkpoints("person/operator", " ").is_err());
    birch
        .resume_checkpoints("person/operator", "the change came from a test fault")
        .unwrap();
    assert!(
        !birch
            .checkpoint_status(next.now_unix_ms, &[])
            .unwrap()
            .halted
    );
    assert_eq!(kinds(&step(&birch, &next)), ["sealed"]);
}

fn authority(store: &Store) -> (String, String) {
    let status = store.replication_status(true, Some(FLEET), &[]).unwrap();
    (status.authority_digest, status.graph_digest)
}

fn claim_ids(store: &Store) -> BTreeSet<String> {
    store
        .claims_page(None, None, 0, None, false, 100_000)
        .unwrap()
        .claims
        .into_iter()
        .map(|claim| claim.id)
        .collect()
}

/// Two nodes agree on a checkpoint until it is stable, without trimming it.
fn stable_pair(alder: &Store, birch: &Store, context: &CheckpointContext) -> String {
    observe(alder, 8);
    observe(birch, 5);
    sync(&[alder, birch]);
    for round in ["sealed", "verified"] {
        for node in [alder, birch] {
            assert_eq!(kinds(&step(node, context)), [round]);
        }
        sync(&[alder, birch]);
    }
    checkpoint_name(newest_due_cut(context.now_unix_ms))
}

#[test]
fn participants_trim_the_stable_checkpoint_and_keep_every_identity() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let cut = newest_due_cut(context.now_unix_ms);
    let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
    let checkpoint = stable_pair(&alder, &birch, &context);
    let before = [&alder, &birch].map(authority);
    let held = claim_ids(&alder);
    let index = alder.index().unwrap();
    for node in [&alder, &birch] {
        assert_eq!(kinds(&step(node, &context)), ["trimmed"]);
        assert!(step(node, &context).is_empty());
    }
    // Every identity stays in the inventory, so the authority digest does not move, and
    // the graph is the same.
    assert_eq!([&alder, &birch].map(authority), before);
    assert_eq!(authority(&alder), authority(&birch));
    let kept = claim_ids(&alder);
    assert!(kept.len() < held.len());
    assert_eq!(kept, claim_ids(&birch));
    let manifests =
        [&alder, &birch].map(|node| node.checkpoint_manifest(&checkpoint, cut).unwrap());
    assert!(!manifests[0].claims.is_empty());
    assert_eq!(manifests[0], manifests[1]);
    for claim in &manifests[0].claims {
        assert!(held.contains(&claim.id) && !kept.contains(&claim.id));
    }
    // Snapshots taken before the trim expire.
    assert!(alder.index().unwrap() > index);
    let status = alder.checkpoint_status(context.now_unix_ms, &[]).unwrap();
    assert_eq!(status.trimmed.as_deref(), Some(checkpoint.as_str()));

    // Both keep replicating new writes.
    observe(&alder, 2);
    observe(&birch, 2);
    sync(&[&alder, &birch]);
    assert_eq!(authority(&alder), authority(&birch));
    assert_eq!(claim_ids(&alder), claim_ids(&birch));
}

#[test]
fn a_trim_that_stops_anywhere_finishes_the_same_after_a_restart() {
    for fault in [
        TrimFault::AfterTombstones,
        TrimFault::AfterChunk(1),
        TrimFault::AfterChunk(2),
        TrimFault::BeforeFinish,
    ] {
        let scratch = tempfile::tempdir().unwrap();
        let context = context(scratch.path(), 0);
        let path = scratch.path().join("alder.sqlite3");
        let alder = Store::open(&path, "alder").unwrap();
        let birch = Store::open_memory("birch").unwrap();
        stable_pair(&alder, &birch, &context);
        assert_eq!(kinds(&step(&birch, &context)), ["trimmed"]);
        alder.set_trim_chunk_envelopes(2);
        alder.set_trim_fault(Some(fault));
        assert!(alder.checkpoint_step(&context).is_err(), "{fault:?}");
        drop(alder);

        let alder = Store::open(&path, "alder").unwrap();
        let actions = step(&alder, &context);
        assert_eq!(kinds(&actions), ["trimmed"], "{fault:?}");
        assert!(step(&alder, &context).is_empty());
        assert_eq!(claim_ids(&alder), claim_ids(&birch), "{fault:?}");
        assert_eq!(authority(&alder), authority(&birch), "{fault:?}");
        let cut = newest_due_cut(context.now_unix_ms);
        let checkpoint = checkpoint_name(cut);
        assert_eq!(
            alder.checkpoint_manifest(&checkpoint, cut).unwrap(),
            birch.checkpoint_manifest(&checkpoint, cut).unwrap(),
            "{fault:?}"
        );
    }
}

#[test]
fn a_node_that_did_not_take_part_adopts_the_manifest_and_nothing_else() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let cut = newest_due_cut(context.now_unix_ms);
    let [alder, birch] = ["alder", "birch"].map(|name| Store::open_memory(name).unwrap());
    let checkpoint = stable_pair(&alder, &birch, &context);
    for node in [&alder, &birch] {
        step(node, &context);
    }
    // A new node gets the kept envelopes, never the dropped ones, and the checkpoint claims.
    let cedar = Store::open_memory("cedar").unwrap();
    sync(&[&alder, &cedar]);
    assert_ne!(authority(&cedar).0, authority(&alder).0);
    assert_eq!(kinds(&step(&cedar, &context)), ["manifest-needed"]);
    assert_eq!(
        cedar
            .checkpoint_manifest_need()
            .unwrap()
            .map(|need| need.checkpoint),
        Some(checkpoint.clone())
    );
    let manifest = alder.checkpoint_manifest(&checkpoint, cut).unwrap();

    // A changed tombstone is refused before anything is stored.
    let mut changed = manifest.clone();
    changed.claims[0]
        .predecessors
        .push("an-invented-claim".into());
    assert_eq!(
        cedar.adopt_checkpoint(&changed).unwrap_err().code,
        "checkpoint-manifest-mismatch"
    );
    assert_eq!(cedar.checkpointed_envelopes().unwrap(), 0);

    assert_eq!(
        kinds(&cedar.adopt_checkpoint(&manifest).unwrap()),
        ["trimmed"]
    );
    assert_eq!(cedar.checkpoint_manifest_need().unwrap(), None);
    assert_eq!(authority(&cedar), authority(&alder));
    assert_eq!(claim_ids(&cedar), claim_ids(&alder));
    assert_eq!(
        cedar.checkpoint_manifest(&checkpoint, cut).unwrap(),
        manifest
    );
    assert!(step(&cedar, &context).is_empty());
}

fn excuse(store: &Store, writer: &str) {
    store
        .excuse_checkpoint_writer(&CheckpointExcuseRequest {
            writer: writer.into(),
            reason: "cut off by a partition".into(),
            actor: "person/operator".into(),
        })
        .unwrap();
}

/// People on each side of a partition excuse the other side, and each side certifies and
/// trims the same cut alone. Once the partition heals, every node applies the same one of
/// the two certificates, and they end with identical tombstones, inventories and graphs,
/// and trim the next checkpoint together.
#[test]
fn both_sides_of_an_excused_partition_converge_on_one_certificate() {
    let scratch = tempfile::tempdir().unwrap();
    let context = context(scratch.path(), 0);
    let cut = newest_due_cut(context.now_unix_ms);
    let checkpoint = checkpoint_name(cut);
    let [alder, birch, cedar, dogwood] =
        ["alder", "birch", "cedar", "dogwood"].map(|name| Store::open_memory(name).unwrap());
    let all = [&alder, &birch, &cedar, &dogwood];
    for node in all {
        observe(node, 4);
    }
    sync(&all);
    let west = [&alder, &birch];
    let east = [&cedar, &dogwood];
    for (side, others) in [(west, ["cedar", "dogwood"]), (east, ["alder", "birch"])] {
        for node in side {
            observe(node, 3);
        }
        for writer in others {
            excuse(side[0], writer);
        }
        sync(&side);
        for round in ["sealed", "verified", "trimmed"] {
            for node in side {
                assert_eq!(kinds(&step(node, &context)), [round], "{}", node.origin);
            }
            sync(&side);
        }
    }
    let [west_certificate, east_certificate] =
        [&alder, &cedar].map(|node| node.trimmed_checkpoint().unwrap().unwrap());
    assert_ne!(west_certificate.drop_digest, east_certificate.drop_digest);

    // The partition heals.
    sync(&all);
    let certified = certificates(&alder.checkpoint_claims().unwrap(), &checkpoint);
    assert_eq!(certified.len(), 2);
    let chosen = chosen_certificate(&certified).unwrap().clone();
    let (kept, switching) = if chosen.terms.drop_digest == west_certificate.drop_digest {
        (west, east)
    } else {
        (east, west)
    };
    for node in kept {
        assert_eq!(node.checkpoint_manifest_need().unwrap(), None);
        assert!(!kinds(&step(node, &context)).contains(&"manifest-needed"));
    }
    let manifest = kept[0].checkpoint_manifest(&checkpoint, cut).unwrap();
    for node in switching {
        assert_eq!(kinds(&step(node, &context)), ["manifest-needed"]);
        let need = node.checkpoint_manifest_need().unwrap().unwrap();
        assert_eq!(need.drop_digest, chosen.terms.drop_digest);
        // The other side's manifest does not verify against the chosen certificate.
        let own = node.checkpoint_manifest(&checkpoint, cut).unwrap();
        assert!(node.adopt_checkpoint(&own).is_err());
        assert_eq!(
            kinds(&node.adopt_checkpoint(&manifest).unwrap()),
            ["trimmed"]
        );
        assert_eq!(
            node.checkpoint_manifest(&checkpoint, cut).unwrap(),
            manifest
        );
    }
    sync(&all);
    for node in all {
        assert_eq!(authority(node), authority(&alder), "{}", node.origin);
        assert_eq!(claim_ids(node), claim_ids(&alder), "{}", node.origin);
        assert_eq!(
            node.checkpoint_manifest(&checkpoint, cut).unwrap(),
            manifest,
            "{}",
            node.origin
        );
        assert_eq!(
            node.trimmed_checkpoint().unwrap().unwrap().drop_digest,
            chosen.terms.drop_digest
        );
        assert!(!kinds(&step(node, &context)).contains(&"manifest-needed"));
    }

    // Every writer seals the next checkpoint, which ends its excusal, and all four trim it
    // together.
    let next = CheckpointContext {
        now_unix_ms: context.now_unix_ms + DAY_MS,
        ..context.clone()
    };
    for node in all {
        observe(node, 2);
    }
    sync(&all);
    for round in ["sealed", "verified", "trimmed"] {
        for node in all {
            assert!(
                kinds(&step(node, &next)).contains(&round),
                "{} did not reach {round}",
                node.origin
            );
        }
        sync(&all);
    }
    for node in all {
        assert_eq!(authority(node), authority(&alder), "{}", node.origin);
        assert_eq!(claim_ids(node), claim_ids(&alder), "{}", node.origin);
    }
    // The older checkpoint's manifest lacks the newer drops, so no node adopts it now.
    assert_eq!(
        alder.adopt_checkpoint(&manifest).unwrap_err().code,
        "checkpoint-superseded"
    );
}

#[test]
fn a_node_that_left_the_fleet_is_not_waited_for() {
    let known = names(&["alder", "birch"]);
    let left = names(&["birch"]);
    assert_eq!(participants(&known, &left, &[]), names(&["alder"]));
}

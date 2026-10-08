use std::collections::BTreeSet;

use serde_json::json;
use smallclaims::store::checkpoint::checkpoint_name;
use smallclaims::store::checkpoint_agreement::{
    CHECKPOINT_EXCUSED, CHECKPOINT_SEALED, CHECKPOINT_VERIFIED, CheckpointClaim, certificates,
    excused_writers_for_rules, first_verifications, participants_for_rules,
};

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn work(writer: &str, rules: &str, verified: bool, members: &[&str]) -> CheckpointClaim {
    let cut = smallclaims::store::checkpoint::DAY_MS;
    let fields = json!({
        "cut_unix_ms":cut,"participants":members,"rules_digest":rules,
        "sealed_digest":"sealed","drop_digest":"drop","retained_digest":"retained",
        "graph_digest":"graph","reader_digest":"reader"
    });
    CheckpointClaim {
        id: format!("{writer}/{rules}/{verified}/{members:?}"),
        kind: if verified {
            CHECKPOINT_VERIFIED
        } else {
            CHECKPOINT_SEALED
        }
        .into(),
        subject: checkpoint_name(cut),
        writer: writer.into(),
        actor: None,
        fields: fields.as_object().unwrap().clone(),
    }
}

fn excuse(writer: &str, actor: Option<&str>) -> CheckpointClaim {
    CheckpointClaim {
        id: format!("excuse/{writer}/{actor:?}"),
        kind: CHECKPOINT_EXCUSED.into(),
        subject: format!("checkpoint-excusal/{writer}"),
        writer: "always-on-a".into(),
        actor: actor.map(Into::into),
        fields: json!({"writer":writer,"reason":"person-authorized offline member"})
            .as_object()
            .unwrap()
            .clone(),
    }
}

#[test]
fn an_excused_offline_writer_with_old_rules_does_not_block_matching_members() {
    let known = names(&["always-on-a", "always-on-b", "cedar"]);
    let always_on = ["always-on-a", "always-on-b"];
    let mut claims = vec![
        work(
            "cedar",
            "rules13",
            false,
            &["always-on-a", "always-on-b", "cedar"],
        ),
        excuse("cedar", Some("person/operator")),
    ];
    assert_eq!(
        participants_for_rules(&known, &BTreeSet::new(), &claims, "rules14"),
        names(&always_on)
    );
    // A genuine returning old-build seal remains in history; it does not undo the decision.
    claims.push(work(
        "cedar",
        "rules13",
        false,
        &["always-on-a", "always-on-b", "cedar"],
    ));
    assert_eq!(
        excused_writers_for_rules(&claims, "rules14"),
        names(&["cedar"])
    );
    assert_eq!(
        participants_for_rules(&known, &BTreeSet::new(), &claims, "rules14"),
        names(&always_on)
    );
    for writer in always_on {
        claims.push(work(writer, "rules14", false, &always_on));
        claims.push(work(writer, "rules14", true, &always_on));
    }
    let certificate = certificates(
        &claims,
        &checkpoint_name(smallclaims::store::checkpoint::DAY_MS),
    );
    assert_eq!(certificate.len(), 1);
    assert_eq!(certificate[0].terms.participants, names(&always_on));
    // A matching self-including seal after normal catch-up resumes participation. The core
    // checkpoint step still applies stable certificates before publishing its first seal.
    claims.push(work(
        "cedar",
        "rules14",
        false,
        &["always-on-a", "always-on-b", "cedar"],
    ));
    assert!(excused_writers_for_rules(&claims, "rules14").is_empty());
    assert_eq!(
        participants_for_rules(&known, &BTreeSet::new(), &claims, "rules14"),
        known
    );
}

#[test]
fn missing_or_untrusted_excusals_and_incoherent_reentry_do_not_reduce_the_roster() {
    let known = names(&["always-on-a", "always-on-b", "cedar"]);
    for actor in [None, Some("agent/worker")] {
        let claims = [excuse("cedar", actor)];
        assert_eq!(
            participants_for_rules(&known, &BTreeSet::new(), &claims, "rules14"),
            known
        );
    }
    // A missing seal or transport outage provides no decision to exclude an always-on peer.
    assert_eq!(
        participants_for_rules(&known, &BTreeSet::new(), &[], "rules14"),
        known
    );
    let mut claims = vec![excuse("cedar", Some("person/operator"))];
    claims.push(work(
        "cedar",
        "rules14",
        false,
        &["always-on-a", "always-on-b"],
    ));
    assert_eq!(
        excused_writers_for_rules(&claims, "rules14"),
        names(&["cedar"])
    );
    let mut malformed = work(
        "cedar",
        "rules14",
        false,
        &["always-on-a", "always-on-b", "cedar"],
    );
    malformed.fields.remove("sealed_digest");
    claims.push(malformed);
    assert_eq!(
        excused_writers_for_rules(&claims, "rules14"),
        names(&["cedar"])
    );
    // All unexcused always-on members remain mandatory even when absent or mismatched.
    assert!(
        participants_for_rules(&known, &BTreeSet::new(), &claims, "rules14")
            .contains("always-on-b")
    );
}

#[test]
fn reentry_does_not_rebind_first_verifications_or_accept_mixed_rules() {
    let all = ["always-on-a", "always-on-b", "cedar"];
    let mut claims = vec![
        work("always-on-a", "rules14", true, &all),
        excuse("cedar", Some("person/operator")),
    ];
    claims.push(work(
        "always-on-a",
        "rules14",
        true,
        &["always-on-a", "always-on-b"],
    ));
    claims.push(work(
        "always-on-b",
        "rules13",
        true,
        &["always-on-a", "always-on-b"],
    ));
    let checkpoint = checkpoint_name(smallclaims::store::checkpoint::DAY_MS);
    let first = first_verifications(&claims, &checkpoint);
    assert_eq!(first["always-on-a"].1.participants, names(&all));
    assert!(certificates(&claims, &checkpoint).is_empty());
    // An excusal cannot rewrite a partially verified cut; normal new-cut/supersession
    // handling remains required. The change never counts mismatched rules as agreement.
}

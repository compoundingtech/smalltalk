//! Exact membership authority and the SQL budget of its signer lookup.
use super::*;
use crate::fleet::{FleetClaim, Membership};
use std::cell::Cell;

thread_local! {
    static STATEMENTS: Cell<usize> = const { Cell::new(0) };
}

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    // Invented admitted facts; signature verification and admission have separate controls.
    connection.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES('fleet_anchor_key','anchor-key');
         CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT, replica_sequence INTEGER);
         CREATE TABLE claims(id TEXT PRIMARY KEY, kind TEXT, subject TEXT, body TEXT, batch_id TEXT);
         CREATE TABLE replica_envelopes(writer TEXT, sequence INTEGER, envelope_hash TEXT,
             batch_id TEXT, PRIMARY KEY(writer,sequence,envelope_hash));
         CREATE INDEX replica_envelopes_batch ON replica_envelopes(batch_id);
         CREATE TABLE replica_envelope_signatures(writer TEXT, sequence INTEGER,
             envelope_hash TEXT, member_key TEXT,
             PRIMARY KEY(writer,sequence,envelope_hash,member_key));"
    ).unwrap();
    connection
}

fn admitted(id: &str, name: &str, key: &str, via: &str, writer: &str, sequence: u64) -> FleetClaim {
    FleetClaim {
        id: id.into(),
        kind: "fleet.member-admitted".into(),
        subject: format!("host/{name}"),
        fields: BTreeMap::from([
            ("fleet_id".into(), serde_json::json!("fleet/example")),
            ("member_key".into(), serde_json::json!(key)),
            ("via".into(), serde_json::json!(via)),
            ("mode".into(), serde_json::json!("listening")),
        ]),
        writer: writer.into(),
        sequence,
        signers: BTreeSet::new(),
    }
}

fn insert_claim(connection: &Connection, claim: &FleetClaim) {
    connection
        .execute(
            "INSERT INTO batches VALUES(?1,?2,?3)",
            params![claim.id, claim.writer, claim.sequence],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO claims VALUES(?1,?2,?3,?4,?1)",
            params![
                claim.id,
                claim.kind,
                claim.subject,
                serde_json::json!({"fields": claim.fields}).to_string()
            ],
        )
        .unwrap();
}

fn envelope(connection: &Connection, claim: &FleetClaim, hash: &str, keys: &[&str]) {
    connection
        .execute(
            "INSERT INTO replica_envelopes VALUES(?1,?2,?3,?4)",
            params![claim.writer, claim.sequence, hash, claim.id],
        )
        .unwrap();
    for key in keys {
        connection
            .execute(
                "INSERT INTO replica_envelope_signatures VALUES(?1,?2,?3,?4)",
                params![claim.writer, claim.sequence, hash, key],
            )
            .unwrap();
    }
}

#[test]
fn membership_join_preserves_forks_exact_signature_identity_and_local_fallback() {
    let connection = fixture();
    let mut anchor = admitted("0-anchor", "birch", "anchor-key", "anchor", "birch", 3);
    let mut unsigned = admitted("1-unsigned", "cedar", "cedar-key", "invite", "birch", 4);
    let mut good = admitted("2-good", "oak", "oak-key", "invite", "birch", 5);
    let mut local = admitted("3-local", "alder", "alder-key", "invite", "birch", 6);
    let remote = admitted("4-remote", "elm", "elm-key", "invite", "oak", 2);
    for claim in [&anchor, &unsigned, &good, &local, &remote] {
        insert_claim(&connection, claim);
    }
    envelope(&connection, &anchor, "fork-one", &["other-key"]);
    envelope(
        &connection,
        &anchor,
        "fork-two",
        &["anchor-key", "other-key"],
    );
    envelope(&connection, &anchor, "fork-empty", &[]);
    envelope(&connection, &unsigned, "unsigned", &["wrong-key"]);
    // A signature with the right key but a different hash, writer, or sequence proves nothing.
    for (writer, sequence, hash) in [
        ("birch", 4, "other-hash"),
        ("oak", 4, "unsigned"),
        ("birch", 99, "unsigned"),
    ] {
        connection
            .execute(
                "INSERT INTO replica_envelope_signatures VALUES(?1,?2,?3,'anchor-key')",
                params![writer, sequence, hash],
            )
            .unwrap();
    }
    envelope(&connection, &good, "good", &["anchor-key"]);
    anchor.signers = BTreeSet::from(["anchor-key".into(), "other-key".into()]);
    unsigned.signers = BTreeSet::from(["wrong-key".into()]);
    good.signers = BTreeSet::from(["anchor-key".into()]);
    let mut expected = vec![anchor, unsigned, good, local.clone(), remote];
    let sealed = fleet_membership_tx(&connection).unwrap();
    assert_eq!(sealed, Membership::fold(Some("anchor-key"), &expected));
    assert!(sealed.counts("0-anchor") && sealed.counts("2-good"));
    assert!(!sealed.counts("1-unsigned") && !sealed.counts("3-local"));
    local.signers.insert("anchor-key".into());
    expected[3] = local;
    let client =
        fleet_membership_tx_with_local_signer(&connection, Some(("birch", "anchor-key"))).unwrap();
    assert_eq!(client, Membership::fold(Some("anchor-key"), &expected));
    assert!(client.counts("3-local"));
    assert!(!client.counts("4-remote"));
}

#[test]
fn membership_without_anchor_does_not_inspect_or_admit_claims() {
    let connection = fixture();
    connection.execute("DELETE FROM meta", []).unwrap();
    let anchor = admitted("anchor", "birch", "anchor-key", "anchor", "birch", 3);
    insert_claim(&connection, &anchor);
    connection
        .execute("UPDATE claims SET body='malformed'", [])
        .unwrap();
    envelope(&connection, &anchor, "anchor", &["anchor-key"]);
    assert_eq!(
        fleet_membership_tx(&connection).unwrap(),
        Membership::default()
    );
}

#[test]
fn membership_signer_lookup_uses_two_statements_independent_of_claim_count() {
    for count in [1, 128] {
        let mut connection = fixture();
        let anchor = admitted("0-anchor", "birch", "anchor-key", "anchor", "birch", 3);
        insert_claim(&connection, &anchor);
        envelope(&connection, &anchor, "anchor", &["anchor-key"]);
        for index in 1..count {
            let claim = admitted(
                &format!("peer-{index}"),
                &format!("cedar-{index}"),
                &format!("key-{index}"),
                "invite",
                "birch",
                3 + index,
            );
            insert_claim(&connection, &claim);
            envelope(
                &connection,
                &claim,
                &format!("envelope-{index}"),
                &["anchor-key"],
            );
        }
        // This connection-local trace does not include other tests or setup statements.
        STATEMENTS.with(|count| count.set(0));
        connection.trace(Some(|_| {
            STATEMENTS.with(|count| count.set(count.get() + 1))
        }));
        let membership = fleet_membership_tx(&connection).unwrap();
        connection.trace(None);
        assert!(membership.counts("0-anchor"));
        STATEMENTS.with(|statements| {
            assert_eq!(
                statements.get(),
                2,
                "membership fold exceeded its two-statement SQL budget at {count} claims"
            )
        });
    }
}

#[test]
fn membership_signers_do_not_multiply_claim_rows_or_scan_unrelated_signatures() {
    use rusqlite::StatementStatus;
    let connection = fixture();
    let anchor = admitted("anchor", "birch", "anchor-key", "anchor", "birch", 3);
    insert_claim(&connection, &anchor);
    envelope(&connection, &anchor, "anchor", &["anchor-key"]);
    for index in 0..128 {
        connection
            .execute(
                "INSERT INTO replica_envelope_signatures VALUES('birch',3,'anchor',?1)",
                [format!("other-key-{index}")],
            )
            .unwrap();
    }
    let measure = || {
        let mut statement = connection
            .prepare(&fleet_claims_with_signers_query())
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        let mut count = 0;
        while let Some(row) = rows.next().unwrap() {
            count += 1;
            let signers: BTreeSet<String> =
                serde_json::from_str(&row.get::<_, String>(7).unwrap()).unwrap();
            assert_eq!(signers.len(), 129);
        }
        drop(rows);
        assert_eq!(count, 1, "signature count must not multiply claim bodies");
        statement.get_status(StatementStatus::VmStep)
    };
    let before = measure();
    for index in 0..4096 {
        connection
            .execute(
                "INSERT INTO replica_envelope_signatures VALUES('elm',?1,'unrelated','other-key')",
                [index],
            )
            .unwrap();
    }
    let after = measure();
    assert!(
        before < 2_000,
        "signer aggregation exceeded its bounded fixture VM budget: {before}"
    );
    assert!(
        after <= before + 32,
        "unrelated signatures increased fold VM work: {before} -> {after}"
    );
}

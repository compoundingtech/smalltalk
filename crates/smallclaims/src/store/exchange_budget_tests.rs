//! Controls for the byte budget of one exchange: a page chosen by count is cut to the leading
//! envelopes whose serialized body fits, measured as serde writes it (base64 payloads, escaped
//! strings, separators and everything else in the exchange), and the rest wait for a later page.
use super::*;
use crate::claim::ReplicaEnvelope;

fn exchange(envelopes: Vec<ReplicaEnvelope>) -> ReplicationExchange {
    let mut exchange: ReplicationExchange = serde_json::from_value(json!({
        "peer": "invented-peer",
        "fleet_id": "invented-fleet",
        "schema_digest": "invented-schema",
        "authority_digest": "invented-authority",
        "graph_digest": "invented-graph",
        "inventory": {"digest": "", "envelopes": []},
    }))
    .unwrap();
    exchange.envelopes = envelopes;
    exchange
}

fn envelope(writer: &str, sequence: u64, payload: Vec<u8>) -> ReplicaEnvelope {
    ReplicaEnvelope {
        writer: writer.into(),
        sequence,
        previous_hash: (sequence > 1).then(|| format!("{:064x}", sequence - 1)),
        hash: format!("{sequence:064x}"),
        accepted_at_unix_ms: 1_792_000_000_000 + u128::from(sequence),
        payload: payload.into(),
        member_key: None,
        signature: None,
    }
}

/// Envelopes whose serialized sizes differ in every way serde can make them differ: empty and
/// binary payloads (base64 grows them by a third), writers whose names need escaping or are
/// multibyte, and a large payload.
fn assorted() -> Vec<ReplicaEnvelope> {
    vec![
        envelope("agent/plain", 1, Vec::new()),
        envelope("agent/plain", 2, b"tiny".to_vec()),
        envelope("agent/quote\"back\\slash\ttab", 3, (0..=255).collect()),
        envelope(
            "agent/multibyte-\u{e9}\u{4e2d}\u{1f600}",
            4,
            vec![0xff; 1000],
        ),
        envelope(
            "agent/plain",
            5,
            (0..70_000_u32).map(|n| (n % 251) as u8).collect(),
        ),
        envelope("agent/plain", 6, b"{\"cbor\":\"is bytes\"}".to_vec()),
    ]
}

fn size(exchange: &ReplicationExchange) -> usize {
    serde_json::to_vec(exchange).unwrap().len()
}

#[test]
fn a_page_that_fits_exactly_is_kept_whole_and_one_byte_less_drops_its_last_envelope() {
    let whole = exchange(assorted());
    let exact = size(&whole);
    let mut fitted = whole.clone();
    fit_exchange_body(&mut fitted, exact).unwrap();
    assert_eq!(fitted.envelopes.len(), 6);
    assert_eq!(size(&fitted), exact);
    let mut short = whole.clone();
    fit_exchange_body(&mut short, exact - 1).unwrap();
    assert_eq!(short.envelopes.len(), 5);
    assert!(size(&short) < exact);
}

#[test]
fn every_budget_keeps_a_prefix_whose_serialized_body_fits() {
    let whole = exchange(assorted());
    let empty = size(&exchange(Vec::new()));
    let first = serde_json::to_vec(&whole.envelopes[0]).unwrap().len();
    // Every envelope's exact boundary and one byte under it, and a stride across the rest.
    let boundaries = (1..=whole.envelopes.len())
        .map(|kept| size(&exchange(whole.envelopes[..kept].to_vec())))
        .flat_map(|exact| [exact - 1, exact]);
    let stride = ((empty + first)..=size(&whole)).step_by(97);
    for budget in boundaries
        .chain(stride)
        .filter(|budget| *budget >= empty + first)
    {
        let mut fitted = whole.clone();
        fit_exchange_body(&mut fitted, budget).unwrap();
        assert!(size(&fitted) <= budget, "budget {budget}");
        let kept = fitted.envelopes.len();
        assert!(kept >= 1);
        let keys = |list: &[ReplicaEnvelope]| {
            list.iter()
                .map(|e| (e.writer.clone(), e.sequence))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&fitted.envelopes),
            keys(&whole.envelopes[..kept]),
            "budget {budget}"
        );
        // The next envelope would not have fit.
        if kept < whole.envelopes.len() {
            let mut one_more = exchange(whole.envelopes[..=kept].to_vec());
            one_more.inventory = fitted.inventory.clone();
            assert!(size(&one_more) > budget, "budget {budget}");
        }
    }
}

#[test]
fn an_envelope_too_large_to_send_alone_is_a_typed_error() {
    let mut oversized = exchange(vec![envelope("agent/plain", 1, vec![7; 10_000])]);
    let budget = size(&oversized) - 1;
    let error = fit_exchange_body(&mut oversized, budget).unwrap_err();
    let error = error.downcast_ref::<St3Error>().unwrap();
    assert_eq!(error.code, "replication-envelope-too-large");
    assert!(error.message.contains("agent/plain 1"), "{}", error.message);
}

#[test]
fn a_backlog_drains_in_pages_within_the_budget_without_duplicates_or_loss() {
    let backlog = (1..=60_u64)
        .map(|sequence| {
            envelope(
                "agent/plain",
                sequence,
                vec![sequence as u8; 500 + 97 * sequence as usize],
            )
        })
        .collect::<Vec<_>>();
    let budget = size(&exchange(Vec::new())) + 40_000;
    let mut remaining = backlog.clone();
    let mut sent = Vec::new();
    let mut pages = 0;
    while !remaining.is_empty() {
        let mut page = exchange(remaining.clone());
        fit_exchange_body(&mut page, budget).unwrap();
        assert!(size(&page) <= budget);
        assert!(
            !page.envelopes.is_empty(),
            "every page moves at least one envelope"
        );
        remaining.drain(..page.envelopes.len());
        sent.extend(page.envelopes);
        pages += 1;
    }
    assert!(pages > 1, "the budget forced more than one page");
    assert_eq!(
        sent.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        backlog.iter().map(|e| e.sequence).collect::<Vec<_>>()
    );
}

#[test]
fn an_exchange_without_envelopes_is_left_alone() {
    let mut empty = exchange(Vec::new());
    let exact = size(&empty);
    fit_exchange_body(&mut empty, exact).unwrap();
    assert!(empty.envelopes.is_empty());
    const { assert!(REPLICATION_EXCHANGE_BODY_BUDGET < crate::sync::MAX_EXCHANGE_BYTES) };
}

#[test]
fn the_cheap_bound_is_never_below_the_measured_size() {
    let mut invalid = envelope("agent/plain", 7, Vec::new());
    invalid.payload = "not base64 \"\\\u{1}\u{e9}".into();
    invalid.member_key = Some("invented-key".into());
    invalid.signature = Some("invented-signature".into());
    for envelope in assorted().iter().chain([&invalid]) {
        let measured = serialized_len(envelope).unwrap();
        assert_eq!(measured, serde_json::to_vec(envelope).unwrap().len());
        assert!(
            envelope_size_bound(envelope) >= measured,
            "{} {}",
            envelope.writer,
            envelope.sequence
        );
    }
}

#[test]
fn cut_exchanges_drain_both_directions_through_the_real_export_with_nothing_lost_or_repeated() {
    const FLEET: &str = "invented-fleet";
    let node = |name: &str| Store::open_memory(name, Arc::new(super::runtime::Plain)).unwrap();
    let alder = node("alder");
    let birch = node("birch");
    for (store, prefix) in [(&alder, "alder"), (&birch, "birch")] {
        store.bind_fleet(FLEET).unwrap();
        for index in 0..16 {
            store
                .append_claim(&ClaimInput {
                    subject: format!("note/{prefix}-{index}"),
                    kind: "example.note".into(),
                    actor: None,
                    fields: BTreeMap::from([("text".into(), json!("x".repeat(2_000)))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
    }
    let keys = |exchange: &ReplicationExchange| {
        exchange
            .envelopes
            .iter()
            .map(|envelope| (envelope.writer.clone(), envelope.sequence))
            .collect::<Vec<_>>()
    };
    // What each side holds that the other lacks, from one unbounded export each way.
    let lacking = |from: &Store, to: &Store| {
        let summary = from.export_replication_summary(FLEET).unwrap();
        let answer = to
            .export_replication_exchange(FLEET, &summary.inventory)
            .unwrap();
        keys(
            &from
                .export_replication_exchange(FLEET, &answer.inventory)
                .unwrap(),
        )
    };
    let mut expected = [lacking(&alder, &birch), lacking(&birch, &alder)];
    assert!(expected.iter().all(|keys| keys.len() >= 4));
    let empty = size(&exchange(Vec::new()));
    // Room for about a third of one side's envelopes per exchange.
    let budget = empty + 16 * 3_000 / 3 + 4_096;
    let mut moved = [Vec::new(), Vec::new()];
    let mut pages = [0, 0];
    for round in 0..60 {
        let mut idle = true;
        for (side, (from, to, relay)) in [(&alder, &birch, "alder"), (&birch, &alder, "birch")]
            .into_iter()
            .enumerate()
        {
            // The answer `to` gives: its real inventory, listing the ranges that differ.
            let summary = from.export_replication_summary(FLEET).unwrap();
            let answer = to
                .export_replication_exchange(FLEET, &summary.inventory)
                .unwrap();
            let page = from
                .export_replication_exchange_within(FLEET, &answer.inventory, &[], budget)
                .unwrap();
            assert!(size(&page) <= budget, "round {round} side {side}");
            if page.envelopes.is_empty() {
                continue;
            }
            idle = false;
            moved[side].extend(keys(&page));
            pages[side] += 1;
            to.receive_replication_exchange(relay, FLEET, &page)
                .unwrap();
        }
        if idle {
            break;
        }
    }
    for side in 0..2 {
        let mut unique = moved[side].clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            moved[side].len(),
            "side {side}: nothing sent twice"
        );
        expected[side].sort();
        assert_eq!(unique, expected[side], "side {side}: everything arrived");
        assert!(
            pages[side] >= 2,
            "side {side}: the budget cut its backlog into pages"
        );
    }
    // Nothing is left: both inventories now agree.
    assert_eq!(
        alder
            .export_replication_summary(FLEET)
            .unwrap()
            .inventory
            .digest,
        birch
            .export_replication_summary(FLEET)
            .unwrap()
            .inventory
            .digest
    );
}

#[test]
fn an_exchange_whose_metadata_alone_passes_the_budget_is_refused_whole() {
    let mut oversized = exchange(vec![envelope("agent/plain", 1, vec![1; 10])]);
    oversized.inventory.envelopes = (0..200)
        .map(|sequence| ReplicaEnvelopeId {
            writer: "agent/plain".into(),
            sequence,
            hash: format!("{sequence:064x}"),
        })
        .collect();
    let budget = size(&exchange(Vec::new())) + 1_000;
    let before = oversized.inventory.envelopes.len();
    let error = fit_exchange_body(&mut oversized, budget).unwrap_err();
    let error = error.downcast_ref::<St3Error>().unwrap();
    assert_eq!(error.code, "replication-exchange-metadata-too-large");
    assert!(
        error.message.contains("listing 200 identities"),
        "{}",
        error.message
    );
    // Nothing was dropped to make it fit.
    assert_eq!(oversized.inventory.envelopes.len(), before);
    assert_eq!(oversized.envelopes.len(), 1);
}

#[test]
fn the_signed_answer_around_a_fitted_exchange_stays_within_the_inflate_limit() {
    // The wrapper's fields do not grow with the exchange; with a long node name and the largest
    // store index it adds far less than the margin the budget leaves.
    let node = "n".repeat(256);
    for envelopes in [Vec::new(), assorted()] {
        let inner = exchange(envelopes);
        let wrapped = crate::sync::PeerResponse::new(&node, u64::MAX, inner.clone());
        let overhead = serialized_len(&wrapped).unwrap() - serialized_len(&inner).unwrap();
        assert!(overhead < 1024, "{overhead}");
        assert!(
            REPLICATION_EXCHANGE_BODY_BUDGET + overhead <= crate::sync::MAX_EXCHANGE_BYTES,
            "{overhead}"
        );
    }
}

//! Helpers the moved tests share (copies of the store tests' own).
#![allow(unused_imports, dead_code)]

use st3_store::store::*;
use crate::model::*;
use crate::store::Store;
use st3_store::store::runtime;

pub const TEST_FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";

pub fn exchange_of(peer: &str, envelopes: Vec<ReplicaEnvelope>) -> ReplicationExchange {
    ReplicationExchange {
        build: None,
        projection_digests: Default::default(),
        peer: peer.into(),
        fleet_id: TEST_FLEET.into(),
        schema_digest: runtime::compatibility_digest(&st3_schema::registry().digest()),
        projection_registry_digest: None,
        projection_versions: Default::default(),
        authority_digest: String::new(),
        graph_digest: String::new(),
        inventory: ReplicationInventory {
            digest: String::new(),
            envelopes: envelopes
                .iter()
                .map(|envelope| ReplicaEnvelopeId {
                    writer: envelope.writer.clone(),
                    sequence: envelope.sequence,
                    hash: envelope.hash.clone(),
                })
                .collect(),
            buckets: Vec::new(),
            accepts: None,
            checkpoint: None,
        },
        envelopes,
        signature_requests: Vec::new(),
        signatures: Vec::new(),
    }
}

pub fn exchange_from(
    source: &Store,
    remote: &ReplicationInventory,
) -> ReplicationExchange {
    source.bind_fleet(TEST_FLEET).expect("source fleet");
    source
        .export_replication_exchange(TEST_FLEET, remote)
        .expect("replication exchange")
}

pub fn publish_mission(store: &Store, source: &str, key: &str) -> MissionSpec {
    let intent = crate::graph::parse_intent(source, "node").expect("mission intent");
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .expect("mission preview");
    store
        .apply(&intent, &planned.subject_tokens, key)
        .expect("mission publish");
    intent
        .missions
        .values()
        .next()
        .expect("published mission")
        .clone()
}

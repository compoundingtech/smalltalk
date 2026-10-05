//! Device signing end to end: a device holds its own P-256 key, enrols it when it pairs, signs a
//! message it sends, and the daemon verifies the signature and attributes the message to the
//! person on that device. The shared vectors in `fixtures/clients/device-signing-v1.json` are the
//! bytes every client must build; the TypeScript and Swift clients test against the same file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use smallclaims::principal::{FIELDS_FORMAT, Verdict, fields_signing_bytes};
use st3::api::AppState;
use st3::store::Store;
use st3_client::{
    Client, ClientError, DeviceSignature, Fence, MessageSendParameters, PairingBegin,
    PairingComplete,
};
use tokio::sync::{Notify, watch};

const PERSON: &str = "person/avery";
const FLEET: &str = "3c9a1f2e-8b7d-4e6c-a5f4-1d2e3c4b5a69";
const SIGNED_FIELDS: [&str; 7] = [
    "content",
    "from",
    "in_reply_to",
    "session_id",
    "tags",
    "title",
    "to",
];

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A phone's key, kept by the phone.
struct Device {
    pair: EcdsaKeyPair,
}

impl Device {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        Self::from_pkcs8(document.as_ref())
    }

    fn from_pkcs8(document: &[u8]) -> Self {
        Self {
            pair: EcdsaKeyPair::from_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                document,
                &SystemRandom::new(),
            )
            .unwrap(),
        }
    }

    fn public(&self) -> String {
        format!("p256:{}", encode(self.pair.public_key().as_ref()))
    }

    fn sign(&self, bytes: &[u8]) -> String {
        encode(
            self.pair
                .sign(&SystemRandom::new(), bytes)
                .unwrap()
                .as_ref(),
        )
    }

    /// Sign a message as the daemon will write it: the subject from the idempotency key, and
    /// every field the daemon stores, missing ones as null and no tags as `[]`.
    fn sign_message(
        &self,
        chain: &[String],
        idempotency_key: &str,
        to: &str,
        content: &str,
        nonce: &str,
        signed_at_unix_ms: u64,
    ) -> DeviceSignature {
        let subject = message_subject(idempotency_key);
        let fields = json!({
            "content": content, "from": PERSON, "in_reply_to": null, "session_id": null,
            "tags": [], "title": null, "to": to,
        });
        let signed_fields = SIGNED_FIELDS.map(str::to_owned).to_vec();
        let bytes = fields_signing_bytes(
            &subject,
            "message.sent",
            Some(PERSON),
            &fields,
            &signed_fields,
            PERSON,
            None,
            &self.public(),
            chain,
            nonce,
            signed_at_unix_ms,
        );
        DeviceSignature {
            signer: PERSON.into(),
            key: self.public(),
            chain: chain.to_vec(),
            nonce: nonce.into(),
            signed_at_unix_ms,
            signature: self.sign(&bytes),
            format: FIELDS_FORMAT.into(),
            signed_fields,
        }
    }
}

fn message_subject(idempotency_key: &str) -> String {
    format!(
        "message/{}",
        &hex::encode(Sha256::digest(idempotency_key.as_bytes()))[..16]
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn shared_completion_persists_both_key_types_and_preserves_a_working_device_on_failure() {
    use st3_client::device::{KeyAlgorithm, Profile, SigningKey, complete};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    // Admit the signer through a pinned fleet anchor, just as an authenticated joining peer
    // learns it. A bare transport copy is insufficient evidence of signature acceptance.
    let anchor = Arc::new(smallclaims::fleet::MemberKey::generate().unwrap().0);
    state.store.pin_fleet_anchor(anchor.public()).unwrap();
    state.store.set_member_key(Some(anchor.clone())).unwrap();
    state.store.append_claim(&st3::model::ClaimInput {
        subject: "host/signing-node".into(), kind: "fleet.member-admitted".into(), actor: None,
        fields: serde_json::from_value(json!({ "fleet_id": FLEET, "member_key": anchor.public(), "via": "anchor", "mode": "listening" })).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
    let socket = root.path().join("st3.sock");
    let served = socket.clone();
    let app = st3::api::router(state.clone());
    let local_server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
    wait_for_socket(&socket).await;
    let local = Client::unix_as(&socket, PERSON);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let gateway = st3::api::fabric_router(state.clone());
    let http_server = tokio::spawn(async move { axum::serve(listener, gateway).await });
    let path = root.path().join("client/devices.json");
    for algorithm in [KeyAlgorithm::Ed25519, KeyAlgorithm::P256] {
        for import in [false, true] {
            let key = if import {
                let document = match algorithm {
                    KeyAlgorithm::Ed25519 => {
                        ring::signature::Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                            .unwrap()
                    }
                    KeyAlgorithm::P256 => EcdsaKeyPair::generate_pkcs8(
                        &ECDSA_P256_SHA256_FIXED_SIGNING,
                        &SystemRandom::new(),
                    )
                    .unwrap(),
                };
                let key_path = root.path().join("import.der");
                std::fs::write(&key_path, document.as_ref()).unwrap();
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                SigningKey::import(algorithm, &key_path).unwrap()
            } else {
                SigningKey::generate(algorithm).unwrap()
            };
            let public = key.public_key().unwrap();
            let challenge = local
                .pairing_begin(&PairingBegin {
                    api_version: st3_client::API_VERSION.into(),
                    device_name: "CLI device".into(),
                    person_id: PERSON.into(),
                    full_control: Some(true),
                    scopes: None,
                })
                .await
                .unwrap()
                .value;
            let previous = std::fs::read(&path).ok();
            let wrong = complete(
                &path,
                &base,
                &challenge.pairing_id,
                "wrong-code",
                key.clone(),
            )
            .await;
            assert!(wrong.is_err());
            assert_eq!(std::fs::read(&path).ok(), previous);
            // A concurrent writer must fail before consuming this still-usable code.
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path.with_extension("lock"))
                .unwrap();
            lock.try_lock().unwrap();
            assert!(
                complete(
                    &path,
                    &base,
                    &challenge.pairing_id,
                    &challenge.code,
                    key.clone()
                )
                .await
                .is_err()
            );
            assert_eq!(std::fs::read(&path).ok(), previous);
            drop(lock);
            let device = complete(
                &path,
                &base,
                &challenge.pairing_id,
                &challenge.code,
                key.clone(),
            )
            .await
            .unwrap();
            assert_eq!(device.session.person_id, PERSON);
            assert_eq!(device.session.device_key_chain.len(), 2);
            // This fixture has no background daemon sealing loop; seal before querying cached verdicts.
            state.store.replication_snapshot().unwrap();
            for grant in &device.session.device_key_chain {
                assert_eq!(state.store.claim_verdict(grant).unwrap(), Verdict::Verified);
            }
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            let saved = std::fs::read(&path).unwrap();
            assert!(
                complete(&path, &base, &challenge.pairing_id, &challenge.code, key)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), saved);
            let loaded = Profile::load(&path).unwrap().unwrap();
            assert_eq!(
                loaded.devices[0]
                    .signing_key
                    .as_ref()
                    .unwrap()
                    .public_key()
                    .unwrap(),
                public
            );
            let client = loaded.clients().pop().unwrap();
            let idem = format!("shared-completion-{algorithm:?}-{import}");
            let snapshot = client.capabilities().await.unwrap().snapshot.id;
            let result = client
                .message_send(
                    format!("action/{idem}"),
                    &idem,
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    MessageSendParameters {
                        to: "agent/alder".into(),
                        content: "a/b \"quoted\"\nhello Ω".into(),
                        title: Some("CLI proof".into()),
                        in_reply_to: None,
                        session_id: None,
                        tags: vec!["zeta".into(), "alpha".into()],
                        attachments: vec![],
                        signature: None,
                    },
                )
                .await
                .unwrap();
            assert!(
                result
                    .value
                    .affected_ids
                    .iter()
                    .any(|id| id == &message_subject(&idem))
            );
            let claim = state
                .store
                .latest_claim(&message_subject(&idem), Some("message.sent"))
                .unwrap()
                .unwrap();
            state.store.replication_snapshot().unwrap();
            assert_eq!(claim.actor.as_deref(), Some(PERSON));
            assert_eq!(
                state.store.claim_signature(&claim.id).unwrap().unwrap().key,
                public
            );
            assert_eq!(
                state.store.claim_verdict(&claim.id).unwrap(),
                Verdict::Verified
            );
            let member = Store::open_memory("peer").unwrap();
            member.bind_fleet(FLEET).unwrap();
            member.pin_fleet_anchor(anchor.public()).unwrap();
            let exchange = state
                .store
                .export_replication_exchange(FLEET, &member.replication_inventory().unwrap())
                .unwrap();
            member
                .receive_replication_exchange("signing-node", FLEET, &exchange)
                .unwrap();
            member.validate_replication_backlog().unwrap();
            member.project_replication_backlog().unwrap();
            member.judge_claims(true).unwrap();
            assert_eq!(member.claim_verdict(&claim.id).unwrap(), Verdict::Verified);
        }
    }
    // A local commit failure after remote success must retain the previous key and credential.
    // The member's consumed code is a separate outcome, not a client-side rollback.
    let saved = std::fs::read(&path).unwrap();
    let challenge = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Unwritable device".into(),
            person_id: PERSON.into(),
            full_control: Some(true),
            scopes: None,
        })
        .await
        .unwrap()
        .value;
    let directory = path.parent().unwrap().to_owned();
    // Refuse a directory another user can write before consuming the still-usable code.
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
    let result = complete(
        &path,
        &base,
        &challenge.pairing_id,
        &challenge.code,
        SigningKey::generate(KeyAlgorithm::P256).unwrap(),
    )
    .await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not be writable by others")
    );
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    let write_failure =
        axum::middleware::map_response(move |response: axum::response::Response| {
            let directory = directory.clone();
            async move {
                if response.status().is_success() {
                    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500))
                        .unwrap();
                }
                response
            }
        });
    let app = st3::api::fabric_router(state.clone()).layer(write_failure);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let blocked_base = format!("http://{}", listener.local_addr().unwrap());
    let blocked_server = tokio::spawn(async move { axum::serve(listener, app).await });
    let result = complete(
        &path,
        &blocked_base,
        &challenge.pairing_id,
        &challenge.code,
        SigningKey::generate(KeyAlgorithm::P256).unwrap(),
    )
    .await;
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("not writable"));
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    assert!(
        Profile::load(&path).unwrap().unwrap().clients()[0]
            .capabilities()
            .await
            .is_ok()
    );
    assert!(
        complete(
            &path,
            &base,
            &challenge.pairing_id,
            &challenge.code,
            SigningKey::generate(KeyAlgorithm::P256).unwrap()
        )
        .await
        .is_err()
    );
    blocked_server.abort();
    let key = SigningKey::generate(KeyAlgorithm::P256).unwrap();
    let public = key.public_key().unwrap();
    let challenge = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Hall display".into(),
            person_id: PERSON.into(),
            full_control: None,
            scopes: Some(vec![
                "read.projections".into(),
                "read.glasses".into(),
                "terminal.read".into(),
            ]),
        })
        .await
        .unwrap()
        .value;
    let device = complete(&path, &base, &challenge.pairing_id, &challenge.code, key)
        .await
        .unwrap();
    assert!(device.signing_key.is_none());
    assert!(device.session.device_key_chain.is_empty());
    assert!(
        Profile::load(&path).unwrap().unwrap().devices[0]
            .signing_key
            .is_none()
    );
    assert!(
        state
            .store
            .claims_for(PERSON, Some(smallclaims::principal::KEY_GRANTED))
            .unwrap()
            .iter()
            .all(|grant| grant.body["fields"]["key"] != public)
    );
    local_server.abort();
    http_server.abort();
}

fn state(root: &Path) -> AppState {
    let store = Store::open_memory("signing-node").unwrap();
    store.bind_fleet(FLEET).unwrap();
    store
        .set_node_key(Arc::new(
            smallclaims::fleet::MemberKey::generate().unwrap().0,
        ))
        .unwrap();
    AppState {
        store: Arc::new(store),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "signing-node".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{} did not appear", socket.display());
}

async fn send(
    client: &Client,
    idempotency_key: &str,
    to: &str,
    content: &str,
    signature: DeviceSignature,
) -> Result<(), ClientError> {
    let snapshot = client.capabilities().await?.snapshot.id;
    client
        .message_send(
            format!("action/{idempotency_key}"),
            idempotency_key,
            Fence {
                snapshot_id: snapshot,
                ..Fence::default()
            },
            MessageSendParameters {
                to: to.into(),
                content: content.into(),
                title: None,
                in_reply_to: None,
                session_id: None,
                tags: vec![],
                attachments: Vec::new(),
                signature: Some(signature),
            },
        )
        .await
        .map(|_| ())
}

fn refused(result: Result<(), ClientError>) -> String {
    match result {
        Err(ClientError::Api(_, message, _)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_message_signed_on_a_device_is_verified_and_attributed_to_its_person() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let socket = root.path().join("st3.sock");
    let app = st3::api::router(state.clone());
    let served = socket.clone();
    let local_server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
    wait_for_socket(&socket).await;
    let local = Client::unix_as(&socket, PERSON);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let gateway = st3::api::fabric_router(state.clone());
    let http_server = tokio::spawn(async move { axum::serve(listener, gateway).await });

    // The phone pairs with its own key and learns the chain it signs with.
    let phone = Device::new();
    let challenge = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Avery's phone".into(),
            person_id: PERSON.into(),
            full_control: Some(true),
            scopes: None,
        })
        .await
        .unwrap();
    let paired = Client::fabric_pairing(&base)
        .pairing_complete(
            &challenge.value.pairing_id,
            &PairingComplete {
                api_version: st3_client::API_VERSION.into(),
                code: challenge.value.code,
                device_public_key: phone.public(),
                key_storage: Some("secure-enclave".into()),
            },
        )
        .await
        .unwrap()
        .value;
    let chain = paired.device_key_chain.clone();
    assert_eq!(
        chain.len(),
        2,
        "the device grant, then the person's root grant"
    );
    let grant = state.store.claim_by_id(&chain[0]).unwrap().unwrap();
    assert_eq!(grant.subject, PERSON);
    assert_eq!(grant.body["fields"]["key"], phone.public());
    assert_eq!(grant.body["fields"]["role"], "device");
    assert_eq!(
        grant.body["fields"]["label"],
        "Avery's phone (secure enclave)"
    );
    let client = Client::fabric_loopback(&base, &paired.credential);

    // A device paired only to read gets no signing key.
    let viewer = Device::new();
    let limited = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Hall display".into(),
            person_id: PERSON.into(),
            full_control: None,
            scopes: None,
        })
        .await
        .unwrap();
    let display = Client::fabric_pairing(&base)
        .pairing_complete(
            &limited.value.pairing_id,
            &PairingComplete {
                api_version: st3_client::API_VERSION.into(),
                code: limited.value.code,
                device_public_key: viewer.public(),
                key_storage: None,
            },
        )
        .await
        .unwrap()
        .value;
    assert!(
        display.device_key_chain.is_empty(),
        "a read-only device signs nothing"
    );
    assert!(
        state
            .store
            .claims_for(PERSON, Some(smallclaims::principal::KEY_GRANTED))
            .unwrap()
            .iter()
            .all(|grant| grant.body["fields"]["key"] != viewer.public())
    );

    // A signed message is written with the phone's signature, and every member verifies it.
    let signature = phone.sign_message(
        &chain,
        "phone-message-send-1",
        "agent/alder",
        "on my way",
        "nonce-phone-send-1",
        now_ms(),
    );
    send(
        &client,
        "phone-message-send-1",
        "agent/alder",
        "on my way",
        signature.clone(),
    )
    .await
    .unwrap();
    let subject = message_subject("phone-message-send-1");
    let claim = state
        .store
        .latest_claim(&subject, Some("message.sent"))
        .unwrap()
        .unwrap();
    assert_eq!(claim.actor.as_deref(), Some(PERSON));
    state.store.replication_snapshot().unwrap();
    let stored = state.store.claim_signature(&claim.id).unwrap().unwrap();
    assert_eq!(stored.key, phone.public());
    assert_eq!(stored.signer, PERSON);
    assert_eq!(
        state.store.claim_verdict(&claim.id).unwrap(),
        Verdict::Verified
    );
    // So does a member that receives it.
    let member = Store::open_memory("member").unwrap();
    member.bind_fleet(FLEET).unwrap();
    let exchange = state
        .store
        .export_replication_exchange(FLEET, &member.replication_inventory().unwrap())
        .unwrap();
    member
        .receive_replication_exchange("signing-node", FLEET, &exchange)
        .unwrap();
    member.validate_replication_backlog().unwrap();
    let copy = member.claim_signature(&claim.id).unwrap().unwrap();
    assert_eq!(copy, stored, "the signature travels with the claim");

    // A retry of the same send gets the first answer.
    send(
        &client,
        "phone-message-send-1",
        "agent/alder",
        "on my way",
        signature.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        state
            .store
            .claims_for(&subject, Some("message.sent"))
            .unwrap()
            .len(),
        1
    );

    // What the daemon refuses before writing anything.
    let tampered = phone.sign_message(
        &chain,
        "phone-message-send-2",
        "agent/alder",
        "on my way",
        "nonce-phone-send-2",
        now_ms(),
    );
    let message = refused(
        send(
            &client,
            "phone-message-send-2",
            "agent/alder",
            "not what was signed",
            tampered,
        )
        .await,
    );
    assert!(message.contains("does not match"), "{message}");
    let replayed = phone.sign_message(
        &chain,
        "phone-message-send-3",
        "agent/alder",
        "again",
        "nonce-phone-send-1",
        now_ms(),
    );
    let message = refused(
        send(
            &client,
            "phone-message-send-3",
            "agent/alder",
            "again",
            replayed,
        )
        .await,
    );
    assert!(message.contains("nonce"), "{message}");
    let stale = phone.sign_message(
        &chain,
        "phone-message-send-4",
        "agent/alder",
        "late",
        "nonce-phone-send-4",
        now_ms() - 60 * 60 * 1000,
    );
    let message = refused(
        send(
            &client,
            "phone-message-send-4",
            "agent/alder",
            "late",
            stale,
        )
        .await,
    );
    assert!(message.contains("15 minutes"), "{message}");
    let loose = phone.sign_message(
        &chain,
        "phone-message-send-5",
        "alder",
        "hi",
        "nonce-phone-send-5",
        now_ms(),
    );
    let message = refused(send(&client, "phone-message-send-5", "alder", "hi", loose).await);
    assert!(message.contains("canonically"), "{message}");
    let stranger = Device::new();
    let forged = stranger.sign_message(
        &chain,
        "phone-message-send-6",
        "agent/alder",
        "hi",
        "nonce-phone-send-6",
        now_ms(),
    );
    let message = refused(send(&client, "phone-message-send-6", "agent/alder", "hi", forged).await);
    assert!(message.contains("not enrolled"), "{message}");
    for key in [
        "phone-message-send-2",
        "phone-message-send-3",
        "phone-message-send-4",
        "phone-message-send-5",
        "phone-message-send-6",
    ] {
        assert!(
            state
                .store
                .latest_claim(&message_subject(key), None)
                .unwrap()
                .is_none()
        );
    }

    // Revoking the pairing revokes the key: nothing it signs from now on verifies.
    let snapshot = local.capabilities().await.unwrap().snapshot.id;
    local
        .pairing_revoke(
            "action/revoke-phone-pairing",
            "revoke-phone-pairing-0001",
            Fence {
                snapshot_id: snapshot,
                ..Fence::default()
            },
            st3_client::TargetParameters {
                target_id: paired.device_id.clone(),
                reason: None,
                evidence: Vec::new(),
                summary: None,
            },
        )
        .await
        .unwrap();
    let revocations = state
        .store
        .claims_for(PERSON, Some(smallclaims::principal::KEY_REVOKED))
        .unwrap();
    assert_eq!(revocations.len(), 1);
    assert_eq!(revocations[0].body["fields"]["key"], phone.public());
    assert_eq!(
        state.store.claim_verdict(&claim.id).unwrap(),
        Verdict::Verified
    );
    // A message the phone signs after the revocation is written, but its verdict is invalid.
    let late = phone.sign_message(
        &chain,
        "phone-message-send-7",
        "agent/alder",
        "after",
        "nonce-phone-send-7",
        now_ms(),
    );
    let late: smallclaims::principal::ClaimSignature =
        serde_json::from_value(serde_json::to_value(&late).unwrap()).unwrap();
    let (written, _) = state
        .store
        .append_signed_claim(
            &st3::model::ClaimInput {
                subject: message_subject("phone-message-send-7"),
                kind: "message.sent".into(),
                actor: Some(PERSON.into()),
                fields: serde_json::from_value(json!({
                    "content": "after", "from": PERSON, "in_reply_to": null, "status": "sent",
                    "tags": [], "title": null, "to": "agent/alder",
                }))
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("phone-message-send-7".into()),
            },
            &late,
        )
        .unwrap();
    state.store.replication_snapshot().unwrap();
    assert!(
        matches!(state.store.claim_verdict(&written.id).unwrap(), Verdict::Invalid(reason) if reason.contains("revoked")),
        "{:?}",
        state.store.claim_verdict(&written.id)
    );

    http_server.abort();
    local_server.abort();
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/clients/device-signing-v1.json")
}

/// A test-only P-256 key, made for these vectors and used nowhere else.
const VECTOR_KEY_PKCS8_HEX: &str = include_str!("../../../fixtures/clients/device-signing-v1.key");

/// The vectors every client checks its signed bytes against. `ST3_WRITE_FIXTURES=1` writes them
/// again (each signature is new; ECDSA is randomized), otherwise the test checks them.
#[test]
fn the_shared_vectors_match_the_bytes_the_daemon_verifies() {
    let device = Device::from_pkcs8(&hex::decode(VECTOR_KEY_PKCS8_HEX.trim()).unwrap());
    let chain = vec![
        "4f6b8a0d2c1e3f5a7b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a".to_owned(),
        "9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d".to_owned(),
    ];
    let cases = [
        (
            "plain",
            "agent/alder",
            "on my way",
            json!(null),
            json!([]),
            json!(null),
        ),
        (
            "escapes",
            "person/robin",
            "a/b \"quoted\"\nnext line 🎉 naïve Ωmega\ttab \\ back",
            json!("Ünïcödé title"),
            json!(["dictated", "zeta", "alpha"]),
            json!("session/phone-1"),
        ),
    ];
    let mut written = Vec::new();
    for (name, to, content, title, tags, session_id) in cases {
        let idempotency_key = format!("vector-message-{name}");
        let subject = message_subject(&idempotency_key);
        let fields = json!({
            "content": content, "from": PERSON, "in_reply_to": null, "session_id": session_id,
            "tags": tags, "title": title, "to": to,
        });
        let signed_fields = SIGNED_FIELDS.map(str::to_owned).to_vec();
        let nonce = format!("vector-nonce-{name}-00");
        let bytes = fields_signing_bytes(
            &subject,
            "message.sent",
            Some(PERSON),
            &fields,
            &signed_fields,
            PERSON,
            None,
            &device.public(),
            &chain,
            &nonce,
            1_791_000_000_000,
        );
        written.push(json!({
            "name": name,
            "idempotency_key": idempotency_key,
            "subject": subject,
            "kind": "message.sent",
            "actor": PERSON,
            "fields": fields,
            "signed_fields": signed_fields,
            "signer": PERSON,
            "key": device.public(),
            "chain": chain,
            "nonce": nonce,
            "signed_at_unix_ms": 1_791_000_000_000_u64,
            "signed_bytes": String::from_utf8(bytes.clone()).unwrap(),
            "signature": device.sign(&bytes),
        }));
    }
    let document = json!({
        "format": FIELDS_FORMAT,
        "about": "Test vectors for device signing (docs/st3/device-signing.md). The key is test-only.",
        "key_pkcs8_hex": VECTOR_KEY_PKCS8_HEX.trim(),
        "cases": written,
    });
    if std::env::var_os("ST3_WRITE_FIXTURES").is_some() {
        std::fs::write(
            fixture_path(),
            serde_json::to_string_pretty(&document).unwrap() + "\n",
        )
        .unwrap();
    }
    let checked: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_path()).unwrap()).unwrap();
    let mut by_name = BTreeMap::new();
    for case in checked["cases"].as_array().unwrap() {
        by_name.insert(case["name"].as_str().unwrap().to_owned(), case.clone());
    }
    for case in document["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let fixture = &by_name[name];
        assert_eq!(
            fixture["signed_bytes"], case["signed_bytes"],
            "{name}: the bytes moved"
        );
        let key = fixture["key"].as_str().unwrap();
        assert!(
            smallclaims::principal::verify_key_signature(
                key,
                fixture["signed_bytes"].as_str().unwrap().as_bytes(),
                fixture["signature"].as_str().unwrap(),
            ),
            "{name}: the fixture's signature does not verify"
        );
    }
}

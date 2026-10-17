//! Split-host report delivery and owned handoff recovery through isolated local APIs.
//! Raw cross-daemon retries deliberately document known limitation #3104, not fleet dedupe.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use st3::api::AppState;
use st3::client::{Client, MessageSendUnconfirmed};
use st3::model::{
    MessageSendRequest, WorkAcknowledgeRequest, WorkHandoffRequest, WorkRequest, WorkStartRequest,
};
use st3::store::Store;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};

const ALDER: &str = "agent/example/alder";
const BIRCH: &str = "agent/example/birch";
const FLEET: &str = "split-host-delivery-fixture";

/// Like message_send_once's in-process daemon, with separate persistent stores and sockets.
struct Host {
    store: Arc<Store>,
    socket: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Host {
    async fn new(root: &Path, name: &str) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let socket = root.join("st3.sock");
        let store = Arc::new(Store::open(&root.join("store.db"), name).unwrap());
        let state = AppState {
            owning_person: None,
            store: store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: name.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&server_socket, st3::api::router(state))
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("isolated daemon did not bind its socket");
        Self { store, socket, server }
    }

    fn client(&self) -> Client {
        Client::unix(&self.socket)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Same response-loss boundary as message_send_once: consume the complete daemon answer,
/// proving application finished, then close the caller connection without returning any bytes.
async fn lose_both_send_answers(front: &Path, back: PathBuf) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(front).unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut caller, _) = listener.accept().await.unwrap();
            let request = read_request(&mut caller).await;
            assert!(request.starts_with(b"POST /v1/messages "));
            let mut daemon = UnixStream::connect(&back).await.unwrap();
            daemon.write_all(&request).await.unwrap();
            let mut answer = Vec::new();
            daemon.read_to_end(&mut answer).await.unwrap();
            assert!(answer.starts_with(b"HTTP/1.1 200"), "{answer:?}");
        }
    })
}

async fn read_request(stream: &mut UnixStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "caller closed before sending a complete request");
        request.extend_from_slice(&buffer[..read]);
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = head.lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
        if request.len() >= end + 4 + length {
            return request;
        }
    }
}

/// Deterministic delivery of real replication envelopes: no fleet services or live sockets.
fn replicate(from: &Host, peer: &str, to: &Host) {
    for _ in 0..100 {
        let exchange = from.store
            .export_replication_exchange(FLEET, &to.store.replication_inventory().unwrap())
            .unwrap();
        if exchange.envelopes.is_empty() {
            return;
        }
        to.store.receive_replication_exchange(peer, FLEET, &exchange).unwrap();
        let admission = to.store.validate_replication_backlog().unwrap();
        assert_eq!((admission.invalid, admission.unknown), (0, 0));
        to.store.apply_replication_repairs().unwrap();
        to.store.project_replication_backlog().unwrap();
    }
    panic!("replication did not drain");
}

fn claim(actor: &str, key: &str) -> WorkRequest {
    WorkRequest {
        wait_ms: None,
        actor: Some(actor.into()),
        incarnation: Some(format!("{actor}-one")),
        summary: None,
        reason: None,
        evidence: vec![],
        idempotency_key: key.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_host_report_and_handoff_documents_known_limitation_3104() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = tempfile::tempdir().unwrap();
        let alder = Host::new(&root.path().join("alder"), "alder").await;
        let birch = Host::new(&root.path().join("birch"), "birch").await;
        let intent = st3::parse_intent(
            "version 2\nagent \"example/alder\" { workspace \"/tmp\"; command \"true\" }\nagent \"example/birch\" { workspace \"/tmp\"; command \"true\" }",
            "alder",
        ).unwrap();
        alder.store.apply_internal(&intent, "fixture-seats").unwrap();
        replicate(&alder, "alder", &birch);

        let report = MessageSendRequest {
            idempotency_key: "split-host-report".into(),
            from: ALDER.into(),
            to: BIRCH.into(),
            content: "Report ready; inspect the delivery fixture.".into(),
            title: Some("Delivery report".into()),
            in_reply_to: None,
            tags: vec![],
            attachments: vec![],
        };
        let front = root.path().join("lossy.sock");
        let proxy = lose_both_send_answers(&front, alder.socket.clone()).await;
        let error = Client::unix(&front).send_message(&report).await.unwrap_err();
        assert!(error.downcast_ref::<MessageSendUnconfirmed>().is_some(), "{error:#}");
        proxy.await.unwrap();
        assert_eq!(alder.store.messages(Some(BIRCH), true).unwrap().len(), 1);

        // Brief 1: both answers were lost, but a same-daemon same-key retry is one message.
        let recovered = alder.client().send_message(&report).await.unwrap();
        assert!(recovered.already_sent);
        assert_eq!(recovered.idempotency_key, report.idempotency_key);
        assert_eq!(alder.store.messages(Some(BIRCH), true).unwrap().len(), 1);

        // Brief 3 / known limitation #3104: a raw retry on another host BEFORE report
        // replication is a new send. This is intentionally not a cross-daemon dedupe promise.
        assert!(birch.store.messages(Some(BIRCH), true).unwrap().is_empty());
        let duplicate = birch.client().send_message(&report).await.unwrap();
        assert!(!duplicate.already_sent);
        assert_eq!(duplicate.idempotency_key, recovered.idempotency_key);
        assert_eq!(duplicate.message.subject, recovered.message.subject);
        let first_claim = alder.store.operation_claim(&report.idempotency_key).unwrap().unwrap();
        let retry_claim = birch.store.operation_claim(&report.idempotency_key).unwrap().unwrap();
        assert_ne!(first_claim.id, retry_claim.id);
        assert_eq!(first_claim.origin, "alder");
        assert_eq!(retry_claim.origin, "birch");
        replicate(&alder, "alder", &birch);
        replicate(&birch, "birch", &alder);
        for host in [&alder, &birch] {
            let reports = host.store.messages(Some(BIRCH), true).unwrap();
            // The key hashes to one subject, so mailbox projection coalesces the two
            // independent sends. That is not a shared acceptance/delivery claim.
            // Fixing #3104 is expected to change this two-origin acceptance assertion.
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].content, report.content);
            let sends = host.store.claims_for_subject_kind_at(
                &recovered.message.subject, "message.sent", None, false, 10,
            ).unwrap();
            assert!(sends.next_cursor.is_none());
            assert_eq!(sends.claims.len(), 2, "#3104: both hosts independently accepted the send");
            assert!(sends.claims.iter().any(|claim| claim.id == first_claim.id));
            assert!(sends.claims.iter().any(|claim| claim.id == retry_claim.id));
        }

        let work = alder.store.start_work(&WorkStartRequest {
            actor: ALDER.into(),
            title: "Inspect the delivery report".into(),
            idempotency_key: "start-report-review".into(),
        }).unwrap();
        alder.store.work_action(&work.subject, "claim", &claim(ALDER, "claim-review")).unwrap();
        let handoff = WorkHandoffRequest {
            actor: ALDER.into(),
            incarnation: Some(format!("{ALDER}-one")),
            to: BIRCH.into(),
            note: "Review the canonical report and acknowledge this owned handoff.".into(),
            evidence: vec![recovered.message.subject.clone()],
            idempotency_key: "handoff-report-review".into(),
        };
        alder.store.handoff_work(&work.subject, &handoff).unwrap();
        replicate(&alder, "alder", &birch);

        // Brief 2: the owner rejects changed input; the replica has the canonical handoff
        // but not the owner's response cache, so it refuses both retries as expired.
        let mut changed = handoff.clone();
        changed.note = "Replace the original report review".into();
        assert_eq!(alder.store.handoff_work(&work.subject, &changed).unwrap_err().code,
            "idempotency-conflict");
        let replay = alder.store.handoff_work(&work.subject, &handoff).unwrap();
        assert_eq!(replay.assigned_to.as_deref(), Some(BIRCH));
        assert!(replay.claimant.is_none());
        for input in [&changed, &handoff] {
            assert_eq!(birch.store.handoff_work(&work.subject, input).unwrap_err().code,
                "idempotency-key-expired");
        }

        // Brief 4: only the owned handoff is exactly once, not the raw report above.
        let handoffs = birch.store.messages(Some(BIRCH), true).unwrap().into_iter()
            .filter(|message| message.tags.contains(&format!("st3-work-handoff:{}", work.subject)))
            .collect::<Vec<_>>();
        assert_eq!(handoffs.len(), 1);
        let message = &handoffs[0];
        assert!(message.content.contains(&handoff.note));
        assert!(!message.content.contains(&changed.note));
        assert_eq!(birch.store.work_action(&work.subject, "claim", &claim(BIRCH, "before-ack"))
            .unwrap_err().code, "handoff-not-acknowledged");
        birch.store.acknowledge_work(&work.subject, &WorkAcknowledgeRequest {
            actor: BIRCH.into(),
            message: message.subject.clone(),
        }).unwrap();
        birch.store.work_action(&work.subject, "claim", &claim(BIRCH, "after-ack")).unwrap();
        replicate(&birch, "birch", &alder);
        for host in [&alder, &birch] {
            assert_eq!(host.store.step_run(&work.subject).unwrap().unwrap().claimant.as_deref(), Some(BIRCH));
            let messages = host.store.messages(Some(BIRCH), true).unwrap();
            assert_eq!(messages.iter().filter(|item| item.subject == message.subject).count(), 1);
        }
    }).await.expect("split-host fixture timed out");
}

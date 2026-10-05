//! Repeatable before/after fixture: real WAL store and Unix WebSockets, separate load process.
//! Run this ignored test alone with --nocapture; ST_MAILBOX_PROFILE_DIR retains raw evidence.
use super::*;
use crate::client::Client;
use tokio_tungstenite::tungstenite::Message;

const SEATS: usize = 128;
const WRITES: usize = 3_000;
const TEST: &str = "api::mailbox::profile::many_streams";

fn claim(subject: String, kind: &str, fields: Value) -> ClaimInput {
    ClaimInput {
        subject,
        kind: kind.into(),
        actor: Some("person/fixture".into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

async fn daemon(root: &Path) {
    crate::profile::init_from_env();
    let mut state = super::super::tests::state(root);
    state.store = Arc::new(Store::open(&root.join("graph.db"), "fixture-host").unwrap());
    for seat in 0..SEATS {
        let subject = format!("agent/fixture-{seat}");
        state
            .store
            .append_claim(&claim(
                subject.clone(),
                "runtime.observed",
                json!({"status":"running","runtime_id":subject,"incarnation_id":"boot"}),
            ))
            .unwrap();
        state
            .store
            .append_claim(&claim(
                subject,
                "harness.observed",
                json!({"state":"ready","driver":"codex","incarnation_id":"boot"}),
            ))
            .unwrap();
    }
    let app = Router::new()
        // Only this isolated fixture bypasses native peer identity. The production stream,
        // fence checks, durable reads and notifications run unchanged.
        .route(
            "/v1/mailbox",
            get(
                |State(state): State<AppState>,
                 Query(fence): Query<Fence>,
                 websocket: WebSocketUpgrade| async move {
                    websocket.on_upgrade(move |socket| stream(state, fence, socket))
                },
            ),
        )
        .route(
            "/v1/mailbox/bind",
            post(
                |State(state): State<AppState>, Json(fence): Json<Fence>| async move {
                    let store = state.store.clone();
                    let bound = blocking_action(move || store.bind_mailbox(&fence)).await?;
                    signal_local_change(&state);
                    Ok::<_, ApiError>(Json(bound))
                },
            ),
        )
        .route(
            "/fixture/claims",
            post(
                |State(state): State<AppState>, Json(input): Json<ClaimInput>| async move {
                    let store = state.store.clone();
                    let claim = blocking_action(move || store.append_claim(&input)).await?;
                    signal_visible_change(&state);
                    Ok::<_, ApiError>(Json(claim))
                },
            ),
        )
        .route("/v1/doctor", get(doctor))
        .route(
            "/fixture/stats",
            get(|| async {
                Json(json!({"cpu_ms":cpu_ms(), "performance":crate::performance::snapshot()}))
            }),
        )
        .layer(from_fn_with_state(
            (state.clone(), ClientTransportBoundary::Unix),
            response_envelope,
        ))
        .with_state(state);
    serve_unix(&root.join("daemon.sock"), app).await.unwrap();
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn frame(socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>) -> Frame {
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

fn percentile(samples: &mut [f64], percent: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() - 1) * percent / 100]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "isolated mailbox CPU/latency profile, run alone"]
async fn many_streams() {
    if let Some(root) = std::env::var_os("ST_MAILBOX_PROFILE_DAEMON") {
        daemon(Path::new(&root)).await;
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = std::env::var_os("ST_MAILBOX_PROFILE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_owned());
    fs::create_dir_all(&root).unwrap();
    let log = fs::File::create(root.join("daemon.log")).unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env("ST_MAILBOX_PROFILE_DAEMON", &root)
            .env("ST3_PROFILE_DIR", root.join("profile"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let socket = root.join("daemon.sock");
    for _ in 0..600 {
        if socket.exists() {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "fixture daemon exited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(socket.exists());
    let client = Client::unix(&socket);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut readers = Vec::new();
    let mut fences = Vec::new();
    for seat in 0..SEATS {
        for component in ["delivery", "title"] {
            let fence: Fence = client
                .post(
                    "/v1/mailbox/bind",
                    &Fence::new(&format!("agent/fixture-{seat}"), "boot", component),
                )
                .await
                .unwrap();
            let mut socket = client.open_mailbox(&fence).await.unwrap();
            if component == "delivery" {
                assert!(
                    matches!(frame(&mut socket).await, Frame::Mailbox { messages } if messages.is_empty())
                );
                fences.push(fence);
            }
            let tx = tx.clone();
            readers.push(tokio::spawn(async move {
                loop {
                    let received = frame(&mut socket).await;
                    let fenced = matches!(received, Frame::Fenced { .. });
                    tx.send((seat, received, Instant::now())).unwrap();
                    if fenced {
                        return;
                    }
                }
            }));
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before: Value = client.get("/fixture/stats").await.unwrap();
    let start = Instant::now();
    let mut delivery = Vec::new();
    let mut writes = Vec::new();
    let mut pace = tokio::time::interval(Duration::from_millis(10));
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    for index in 0..WRITES {
        pace.tick().await;
        let targeted = index % 30 == 0;
        let seat = (index / 30) % SEATS;
        let subject = format!("message/fixture-{index}");
        let sent = Instant::now();
        let _: ClaimRecord = client.post("/fixture/claims", &claim(subject.clone(), "message.sent",
            json!({"status":"sent","from":"person/fixture","to":if targeted { format!("agent/fixture-{seat}") } else { "agent/elsewhere".into() },"content":"Fixture note."}))).await.unwrap();
        writes.push(sent.elapsed().as_secs_f64() * 1000.0);
        if targeted {
            loop {
                let (recipient, received, at) =
                    tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                if recipient == seat
                    && matches!(received, Frame::Mailbox { messages } if messages.iter().any(|message| message.subject == subject))
                {
                    delivery.push(at.duration_since(sent).as_secs_f64() * 1000.0);
                    break;
                }
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after: Value = client.get("/fixture/stats").await.unwrap();
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let doctor: Value = client.get("/v1/doctor").await.unwrap();
    fs::write(
        root.join("doctor.json"),
        serde_json::to_vec_pretty(&doctor).unwrap(),
    )
    .unwrap();
    let mut ownership = Vec::new();
    for old in fences.iter().take(32) {
        let started = Instant::now();
        let _: Fence = client
            .post(
                "/v1/mailbox/bind",
                &Fence::new(&old.subject, "boot", "delivery"),
            )
            .await
            .unwrap();
        loop {
            let (_, received, at) = tokio::time::timeout(Duration::from_secs(12), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(received, Frame::Fenced { .. }) {
                ownership.push(at.duration_since(started).as_secs_f64() * 1000.0);
                break;
            }
        }
    }
    let result = json!({"seats":SEATS,"streams":SEATS*2,"writes":WRITES,"targeted_messages":delivery.len(),
        "wall_ms":wall_ms,"daemon_cpu_ms":after["cpu_ms"].as_f64().unwrap()-before["cpu_ms"].as_f64().unwrap(),
        "delivery_p50_ms":percentile(&mut delivery,50),"delivery_p95_ms":percentile(&mut delivery,95),"delivery_max_ms":percentile(&mut delivery,100),
        "ownership_p50_ms":percentile(&mut ownership,50),"ownership_p95_ms":percentile(&mut ownership,95),"ownership_max_ms":percentile(&mut ownership,100),
        "write_p95_ms":percentile(&mut writes,95),"before":before["performance"],"after":after["performance"]});
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{result}");
    for reader in readers {
        reader.abort();
    }
}

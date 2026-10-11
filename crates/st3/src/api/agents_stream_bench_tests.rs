pub(super) fn agents_stream_bench_buffer(fd: std::os::fd::RawFd, option: libc::c_int) {
    let bytes: libc::c_int = 32 * 1024;
    // SAFETY: the live socket descriptor is borrowed, and the pointer and length describe
    // one initialized socket-option integer. No ownership or lifetime is transferred.
    assert_eq!(unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, option,
        (&bytes as *const libc::c_int).cast(), std::mem::size_of_val(&bytes) as libc::socklen_t) }, 0);
}

// Included inside api::tests: reuse the calibrated real-roster owner fixture above.
#[derive(Default)]
struct AgentsStreamBenchStats {
    frames: usize,
    sequences: usize,
    snapshots: usize,
    deltas: usize,
    bytes: usize,
    latencies_ms: Vec<f64>,
    last_revision: u64,
    last_cut: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "release-only isolated-process transport measurement; run explicitly with --ignored --nocapture --test-threads=1"]
async fn agents_stream_calibrated_rate_latency_and_retention() {
    use std::os::fd::AsRawFd as _;
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};
    if cfg!(debug_assertions) { panic!("measure a release build in an isolated test process"); }
    let subscribers: usize = std::env::var("ST_AGENTS_STREAM_SUBSCRIBERS").unwrap_or_else(|_| "1".into()).parse().unwrap();
    assert!([0, 1, 4, 16].contains(&subscribers));
    let revisioned = std::env::var("ST_AGENTS_STREAM_MODE").unwrap_or_else(|_| "revisioned".into()) == "revisioned";
    let mode = if revisioned { "revisioned" } else { "legacy" };
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(Store::open(&root.path().join("stream.sqlite3"), "node").unwrap());
    let store = Arc::clone(&state.store);
    let empty_rss = publication_process_memory_kib().0;
    let mut agents = publication_bench_seed(&store, 0..200);
    let _warm_demand = store.subscribe_agents_publications();
    start_agent_roster(&state);
    let publication = publication_bench_calibrate(&store, &mut agents, 0).await;
    publication_report_distribution("stream calibrated 200 current agents", &publication);
    assert_eq!(publication.rows().len(), 200);
    drop(publication);

    let phases = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let notices = Arc::new(parking_lot::Mutex::new((std::collections::HashMap::<u64, Instant>::new(),
        std::collections::HashMap::<u64, Instant>::new(), Vec::<std::sync::Weak<crate::store::agents_publication::AgentsPublication>>::new(), [0_usize; 4])));
    let mut receiver = store.subscribe_agents_publications();
    let tracker = tokio::spawn({
        let (notices, phases) = (notices.clone(), phases.clone());
        async move {
            while receiver.changed().await.is_ok() {
                let Some(latest) = receiver.borrow_and_update().clone() else { continue; };
                let mut captured = notices.lock();
                let now = Instant::now();
                captured.0.insert(latest.metadata().revision, now);
                captured.1.insert(latest.metadata().status_watermark.store_index, now);
                captured.2.push(Arc::downgrade(&latest));
                captured.3[phases.load(std::sync::atomic::Ordering::Acquire)] += 1;
            }
        }
    });
    let app = axum::Router::new().route("/stream", axum::routing::get(client_v0::collection_stream))
        .layer(axum::Extension(client_v0::ClientSession::local(None).unwrap())).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // Small fixed transport buffers make a five-second non-reader exert actual
    // backpressure, rather than hiding an arbitrary publication queue in TCP.
    agents_stream_bench_buffer(listener.as_raw_fd(), libc::SO_SNDBUF);
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let mut readers = Vec::new();
    let mut totals = Vec::new();
    let (resume, paused) = tokio::sync::watch::channel(true);
    let stalled = Arc::new(tokio::sync::Notify::new());
    let latest_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    for subscriber in 0..subscribers {
        let mut request = format!("ws://{address}/stream").into_client_request().unwrap();
        request.headers_mut().insert("sec-websocket-protocol", "st3.client.collections.v0".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        if let tokio_tungstenite::MaybeTlsStream::Plain(stream) = socket.get_ref() {
            agents_stream_bench_buffer(stream.as_raw_fd(), libc::SO_RCVBUF);
        }
        let command = if revisioned {
            json!({"kind":"subscribe","id":"agents","collection":"agents","agents_publication_version":1})
        } else { json!({"kind":"subscribe","id":"agents","collection":"agents","limit":200}) };
        socket.send(Message::Text(command.to_string().into())).await.unwrap();
        let counters = Arc::new(parking_lot::Mutex::new(std::array::from_fn::<_, 4, _>(|_| AgentsStreamBenchStats::default())));
        totals.push(counters.clone());
        let (ready, initial) = tokio::sync::oneshot::channel();
        let (phases, notices, stalled, latest_seen) = (phases.clone(), notices.clone(), stalled.clone(), latest_seen.clone());
        let mut paused = paused.clone();
        readers.push(tokio::spawn(async move {
            let mut ready = Some(ready);
            let mut applied = 0_u64;
            let mut epoch = None;
            let mut chunk = 0_u64;
            loop {
                if subscriber == 0 && !*paused.borrow_and_update() {
                    stalled.notify_one();
                    paused.wait_for(|resumed| *resumed).await.unwrap();
                }
                let message = tokio::select! {
                    biased;
                    changed = paused.changed(), if subscriber == 0 => { if changed.is_err() { return; } continue; }
                    message = socket.next() => message,
                };
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(payload))) => {
                        socket.send(Message::Pong(payload)).await.unwrap();
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    other => panic!("the measurement socket must stay live: {other:?}"),
                };
                let frame: Value = serde_json::from_str(&text).unwrap();
                let snapshot = frame["kind"] == "snapshot";
                let delta = frame["kind"] == "changes";
                assert!(snapshot || delta, "measurement subscription failed: {frame}");
                assert!(text.len() <= CLIENT_MAX_RESPONSE_BYTES);
                assert_eq!(frame["has_more"], false);
                let complete = if revisioned {
                    let next_epoch = frame["publication"]["node_epoch"].as_str().unwrap().to_owned();
                    let target = frame["publication"]["revision"].as_u64().unwrap();
                    if delta {
                        assert_eq!(epoch.as_ref(), Some(&next_epoch));
                        assert_eq!(frame["base_revision"], applied);
                        applied = target;
                        true
                    } else {
                        assert_eq!(frame["chunk_index"], chunk);
                        chunk += 1;
                        let complete = chunk == frame["chunk_count"].as_u64().unwrap();
                        if complete { chunk = 0; applied = target; epoch = Some(next_epoch); }
                        complete
                    }
                } else { true };
                let cut = frame["snapshot"]["store_index"].as_u64().unwrap();
                let notice = {
                    let captured = notices.lock();
                    if revisioned { captured.0.get(&applied).copied() } else { captured.1.get(&cut).copied() }
                };
                let mut counters = counters.lock();
                let counters = &mut counters[phases.load(std::sync::atomic::Ordering::Acquire)];
                counters.frames += 1;
                counters.bytes += text.len();
                counters.snapshots += usize::from(snapshot);
                counters.deltas += usize::from(delta);
                if complete {
                    counters.sequences += 1;
                    counters.last_revision = applied;
                    counters.last_cut = cut;
                    if let Some(notice) = notice { counters.latencies_ms.push(notice.elapsed().as_secs_f64() * 1000.0); }
                    if subscriber == 0 { latest_seen.store(cut, std::sync::atomic::Ordering::Release); }
                    if let Some(ready) = ready.take() {
                        assert_eq!(frame["order"].as_array().unwrap().len(), 200);
                        let _ = ready.send(());
                    }
                }
            }
        }));
        tokio::time::timeout(Duration::from_secs(15), initial).await.unwrap().unwrap();
    }
    let baseline_rss = publication_process_memory_kib().0;
    let phase_seconds = std::env::var("ST_AGENTS_STREAM_SECONDS").ok().map(|value| value.parse::<u64>().unwrap());
    let durations = phase_seconds.map_or([60, 120, 120, 10], |seconds| [seconds; 4]);
    let mut turn = 0_usize;
    for (phase, name) in ["quiet", "paced", "burst", "recovery"].into_iter().enumerate() {
        phases.store(phase, std::sync::atomic::Ordering::Release);
        let started = Instant::now();
        let cpu = client_v0::stream_start_tests::process_cpu_ms();
        let notice_start = notices.lock().3[phase];
        let mut rss = Vec::new();
        let mut live_publications = 0;
        let mut writes = 0_usize;
        let mut last_write = Instant::now() - Duration::from_secs(2);
        let mut last_sample = Instant::now() - Duration::from_secs(1);
        let mut resumed = false;
        if phase == 2 && subscribers > 0 {
            resume.send_replace(false);
            tokio::time::timeout(Duration::from_secs(2), stalled.notified()).await.unwrap();
        }
        let fresh = if phase == 2 {
            Some(tokio::spawn({
                let state = state.clone();
                async move {
                    loop {
                        let _ = client_agents(State(state.clone()), Extension(new_client_snapshot(&state)),
                            Query(ClientListQuery { fresh:true, ..ClientListQuery::default() })).await.unwrap();
                        tokio::task::yield_now().await;
                    }
                }
            }))
        } else { None };
        while started.elapsed() < Duration::from_secs(durations[phase]) {
            let pace = if phase == 2 { Duration::from_millis(20) } else { Duration::from_secs(1) };
            if (phase == 1 || phase == 2) && last_write.elapsed() >= pace {
                let agent = turn % agents.len();
                roster_fixture_append(&store, &publication_bench_subject(agent), "harness.todo.observed",
                    publication_todo(&format!("publication-bench-{agent}"), &agents[agent].texts,
                        (turn / agents.len()).is_multiple_of(2)));
                turn += 1;
                writes += 1;
                last_write = Instant::now();
            }
            if phase == 2 && !resumed && started.elapsed() >= Duration::from_secs(5.min(durations[phase] / 2)) {
                resume.send_replace(true);
                resumed = true;
            }
            if last_sample.elapsed() >= Duration::from_millis(250) {
                rss.push(publication_process_memory_kib().0);
                let alive = notices.lock().2.iter().filter(|publication| publication.strong_count() != 0).count();
                live_publications = live_publications.max(alive);
                last_sample = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if let Some(fresh) = fresh { fresh.abort(); let _ = fresh.await; }
        if phase == 2 && subscribers > 0 {
            resume.send_replace(true);
            let wanted = store.index().unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while latest_seen.load(std::sync::atomic::Ordering::Acquire) < wanted {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("the stalled reader must converge to the latest publication after resuming");
        }
        let seconds = started.elapsed().as_secs_f64();
        let publications = notices.lock().3[phase] - notice_start;
        let cpu_ms = client_v0::stream_start_tests::process_cpu_ms() - cpu;
        let (_, peak) = publication_process_memory_kib();
        println!("agents stream process mode={mode} subscribers={subscribers} phase={name} seconds={seconds:.3} rows=200 writes={writes} publications={publications} publications_per_s={:.3} cpu_ms={cpu_ms:.3} rss_empty_kib={empty_rss} rss_subscribed_kib={baseline_rss} rss_min_kib={} rss_max_kib={} rss_peak_kib={peak} live_complete_publications_max={live_publications}",
            publications as f64 / seconds, rss.iter().min().copied().unwrap_or_default(), rss.iter().max().copied().unwrap_or_default());
        for (subscriber, counters) in totals.iter().enumerate() {
            let mut counters = counters.lock();
            let counters = &mut counters[phase];
            counters.latencies_ms.sort_by(f64::total_cmp);
            let quantile = |q: f64| counters.latencies_ms.get(((counters.latencies_ms.len() as f64 * q) as usize)
                .min(counters.latencies_ms.len().saturating_sub(1))).copied().unwrap_or_default();
            println!("agents stream socket mode={mode} subscribers={subscribers} socket={subscriber} phase={name} stalled={} frames={} sequences={} snapshots={} deltas={} bytes={} sequences_per_s={:.3} frames_per_s={:.3} notice_latency_samples={} notice_to_frame_p50_ms={:.3} notice_to_frame_p95_ms={:.3} last_revision={} last_cut={}",
                phase == 2 && subscriber == 0, counters.frames, counters.sequences, counters.snapshots, counters.deltas,
                counters.bytes, counters.sequences as f64 / seconds, counters.frames as f64 / seconds,
                counters.latencies_ms.len(), quantile(0.5), quantile(0.95), counters.last_revision, counters.last_cut);
            if revisioned && !(phase == 2 && subscriber == 0) && !counters.latencies_ms.is_empty() {
                assert!(quantile(0.95) < 1000.0, "healthy publication delivery must stay below one second");
            }
        }
        assert!(live_publications <= subscribers + 3, "one shared current publication and at most one full in-flight publication per subscriber, plus temporary producer/observer captures");
    }
    for reader in readers { reader.abort(); let _ = reader.await; }
    server.abort(); let _ = server.await;
    tracker.abort(); let _ = tracker.await;
}

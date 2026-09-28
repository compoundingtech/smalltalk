//! A Codex seat's live thread binding after the TUI switches to another thread.

use std::os::unix::net::UnixListener;

use super::*;

/// Accept one control connection and play `frames` to it, then close.
fn serve(listener: UnixListener, frames: Vec<Value>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        for frame in frames {
            write_json_message(&mut websocket, &frame).unwrap();
        }
        // Leave the frames time to be read before the close.
        thread::sleep(Duration::from_millis(300));
        let _ = websocket.close(None);
        let _ = websocket.flush();
    })
}

/// After `/new` (or `/resume` of another conversation) in the seat's TUI, the thread that the
/// person and the model are working in is the new one. The seat's binding, its observed state,
/// and therefore its delivery target must move with it.
#[test]
#[ignore = "fails on main: the Codex control pump binds the first thread only, so a thread the TUI switches to is never observed and the binding stays on the abandoned thread"]
fn a_thread_the_tui_switches_to_becomes_the_seats_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("server.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = serve(
        listener,
        vec![
            json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-first", "status": { "type": "idle" } } }
            }),
            json!({
                "method": "turn/started",
                "params": { "threadId": "thread-first", "turn": { "id": "turn-first" } }
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-first",
                    "turn": { "id": "turn-first", "status": "completed" }
                }
            }),
            // The person types `/new`: the TUI starts a fresh thread and works in it.
            json!({
                "method": "thread/started",
                "params": { "thread": { "id": "thread-next", "status": { "type": "idle" } } }
            }),
            json!({
                "method": "turn/started",
                "params": { "threadId": "thread-next", "turn": { "id": "turn-next" } }
            }),
        ],
    );
    let stream = UnixStream::connect(&socket).unwrap();
    let (websocket, _) = tungstenite::client("ws://localhost/", stream).unwrap();
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let binding_path = state_dir.join("binding.json");
    let control_state_path = state_dir.join("control-state.json");
    let runtime = CodexRuntime::fresh("h.worker".into(), "h.worker".into()).unwrap();
    let (tx, rx) = mpsc::channel();
    let pump = {
        let runtime = runtime.clone();
        let binding_path = binding_path.clone();
        let control_state_path = control_state_path.clone();
        thread::spawn(move || {
            pump_control(
                websocket,
                &binding_path,
                &control_state_path,
                &runtime,
                None,
                None,
                Arc::new(AtomicBool::new(false)),
                tx,
            )
        })
    };
    server.join().unwrap();
    pump.join().unwrap();
    let events = rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ControlEvent::Bound)),
        "{events:?}"
    );

    let binding = load_current_binding(&binding_path, &runtime)
        .unwrap()
        .unwrap();
    let state = load_current_control_state(&control_state_path, &runtime, &binding)
        .unwrap()
        .unwrap();
    assert_eq!(
        binding.thread_id(),
        "thread-next",
        "the seat stays bound to the thread the TUI left (observed {:?})",
        state.observed()
    );
    assert_eq!(
        state.observed(),
        &CodexObservedState::Active {
            turn_id: "turn-next".into()
        }
    );
}

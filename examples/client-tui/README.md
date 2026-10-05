# Small client TUI

A minimal Rust client assembled from `st3-client`, `st3-feed` and
`st3-conversation-ui` with its optional ratatui drawing. It imports no stui or daemon code.

Read [Build your own client](../../docs/clients/build-your-own.md) for setup, the package map,
transport/authentication choices and extension points.

```sh
cargo run --locked -p st3-client-tui -- person/avery [SOCKET]
cargo test --locked -p st3-client-tui
```

Replace the invented person with one in your fleet. Omit `[SOCKET]` to use normal st discovery.
Up/Down selects an agent; type then Enter sends; Esc clears; Ctrl-C quits. The client follows
the recent conversation, preserves selection across updates, disables sending while offline
and retains action identity when retrying an uncertain send. Acceptance is not delivery.

`src/main.rs` owns terminal/input lifecycle. `src/lib.rs` composes public updates/models/drawing
and typed sends. `tests/client.rs` exercises them with a terminal backend and test gateway.
Workspace CI builds and tests this example on Linux and macOS; it is not shipped as an st command.

# st3-client

Reusable typed Rust client for `st3.client.v0`. It consumes versioned JSON projections and actions;
it never parses st3 CLI output, KDL, Markdown, raw claim envelopes, or harness transcripts.

```rust
let client = st3_client::Client::unix("/path/to/st3.sock");
let capabilities = client.capabilities().await?;
let work = client.work_list(None, Some(100), false).await?;
```

Use `Client::unix_gateway` with a pairing credential for a paired Unix gateway, or
`Client::fabric_loopback` with an HTTP(S) gateway URL and pairing credential for remote access.
The HTTP(S) transport does not require Fabric; a reachable authenticated gateway is sufficient.
Every manifest read and mutation has a named typed method; raw collection strings, paths, JSON
parameters, and manually paired action discriminators are private implementation details.
Mutation callers pass the latest response snapshot and exact resource fences. `ClientError::Api`
preserves stable error codes such as `stale-fence`, `runtime-not-local`, and `cursor-gap`.

## Raw PTY connectors

`raw_terminal_attachment(terminal_id, runtime_incarnation, RawTerminalMode::Attach)` acquires
a single-use capability fenced to the terminal's current runtime incarnation. `RawTerminalMode::Peek`
requests a read-only connection instead. The method extracts a `RawTerminalAttachment` from
the standard client-v0 response envelope: it carries the terminal ID, incarnation, owner host,
mode, and stream capability.
The normal terminal stream authentication and terminal access rules also apply to raw streams.

`raw_terminal_stream(&attachment)` consumes that capability at the gateway and returns a
`tokio::net::UnixStream`, usable as a `pty-terminal` connector. It negotiates `st3.client.pty.v0`
with the capability in the secondary `st3.cap.<capability>` WebSocket subprotocol.
Both trusted local Unix and paired Unix/HTTP(S) clients expose the same connector.

```rust
let attachment = client
    .raw_terminal_attachment(
        "terminal/demo",
        "demo-runtime:incarnation",
        st3_client::RawTerminalMode::Attach,
    )
    .await?;
let connector = client.raw_terminal_stream(&attachment).await?;
// Give connector to pty-terminal; the consumer sends its own ATTACH frame.
```

The bridge carries binary PTY bytes unchanged in both directions: it does not decode PTY
frames, send ATTACH/PEEK, or turn output into screen projections. WebSocket message boundaries
are not PTY frame boundaries. Buffering is bounded and a slow consumer applies backpressure.
Dropping the connector closes the remote attachment and cancels the bridge, including when a
write is stalled. A closed remote stream produces EOF on the connector.

Reconnect by explicitly acquiring a **new** attachment and opening a new connector; capabilities
cannot be replayed, and the client does not transparently reconnect or reuse one.

### Foreground observation leases

PEEK streams expire after 60 seconds without selected foreground observation and after
300 seconds absolute. PTY traffic does not renew them. Use
`raw_terminal_stream_controlled(&attachment)` to obtain
`RawTerminalStream { stream, activity }`; clone `activity` and call
`activity.selected_use().await` only while the terminal is selected in the foreground
(approximately every 20 seconds). Stop sending controls on deselection or backgrounding.
The byte-only connector has no automatic renewal and remains idle-expiring.

Rotate before the absolute deadline by acquiring a fresh capability and connector. Send the
existing PEEK initialization frame and receive fresh GEOMETRY and SCREEN before replacing
the visible surface; then drop the old connector. Expired streams cannot be extended.
Selected-use success means the control was sent, not that renewal was acknowledged.
Revocation or lost authority watches closes existing streams; recover through fresh
attachment/readiness APIs, never by replaying an old capability.

Paired observations require the pairing issuer's gateway. `pairing.revoke` on another
member fails without committing, with HTTP 409 `issuer-required` and
`details.issuer_host_id`. Automatic cross-gateway revocation routing is not provided.

Run `cargo run -p st3-client-codegen -- --check` from the repository root to verify the generated
models and operation surfaces exactly match the normative schema and operation manifest.

# st3-client

Reusable typed Rust client for `st3.client.v0`. It consumes versioned JSON projections and actions;
it never parses st3 CLI output, KDL, Markdown, raw claim envelopes, or harness transcripts.

```rust
let client = st3_client::Client::unix("/path/to/st3.sock");
let capabilities = client.capabilities().await?;
let work = client.work_list(None, Some(100), false).await?;
```

Use `Client::fabric_loopback` with a pairing credential for remote access. Every manifest read and
mutation has a named typed method; raw collection strings, paths, JSON parameters, and manually
paired action discriminators are private implementation details. Mutation callers pass the latest
response snapshot and exact resource fences. `ClientError::Api` preserves stable error codes such
as `stale-fence`, `runtime-not-local`, and `cursor-gap`.

Run `cargo run -p st3-client-codegen -- --check` from the repository root to verify the generated
models and operation surfaces exactly match the normative schema and operation manifest.

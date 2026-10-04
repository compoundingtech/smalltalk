# Smalltalk feed

`st3-feed` owns the live collection subscriptions, member reconnects, snapshot model and private
display cache used by stui. It depends on the generated `st3-client` and the conversation model;
its normal dependencies contain no terminal renderer. It changes no server protocol.

## Start a feed

Supply a `Client` for each member, already configured with that member's identity and credentials.
`run_members` keeps one collection-stream socket on the selected member, tries every supplied
member before waiting, and rotates to the next member when a socket drops. Credentials remain
on their own clients. `run` is the convenience form for one local member without glasses.

```rust,no_run
use st3_client::Client;
use st3_feed::{Command, Update};

#[tokio::main]
async fn main() {
    let (updates, received) = std::sync::mpsc::channel();
    let (commands, command_receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(st3_feed::run_members(
        vec![Client::unix_as("/tmp/example.sock", "person/avery")],
        false, // local connection; true identifies a remote device gateway
        false, // request the optional glasses window
        updates,
        command_receiver,
    ));
    commands.send(Command::Converse { targets: vec!["agent/demo".into()] }).unwrap();
    // Drain `received.try_recv()` from the application's update loop.
    if let Ok(Update::Window { window, snapshot, items, has_more }) = received.try_recv() {
        // Replace this window's items and snapshot; preserve st's order and truncation marker.
        let _ = (window, snapshot, items, has_more);
    }
    drop(commands); // closes the feed; never queues a mutation
    task.await.unwrap();
}
```

## Subscriptions and updates

| Caller subscribes to | Updates it receives |
| --- | --- |
| Attention, missions and agents automatically | `Window` replaces the entire joined window, with its snapshot and `has_more`; `WindowFailed` leaves the last successful items visible. Each window holds at most 200 rows. |
| Glasses via the `glasses` argument, if granted at version 1 or newer | `GlassesVersion` precedes the glasses `Window`, so the application can select the matching storage shape. The window holds at most 100 glasses. |
| `Converse { targets }` | Up to `MAX_CONVERSATIONS` (three) agent or session conversations. `Conversation` identifies its target and resolved session; `replace` resets the newest page, otherwise merge entries by ID. Preserve `has_more` for older-page navigation. `ConversationFailed` retries unless history is permanently incomplete. |
| `Resubscribe { target }` | Resolve a followed agent's current session again after a restart; subsequent conversation updates replace the previous page. |
| `Follow { runtime_ids }` | One runtime's terminal: `Attached` supplies its viewer and incarnation, followed by `Screen`. Transient failures send `Reconnecting`; `Ended` reports a refusal, process exit or changed incarnation. `Unfollow` ends following. |

`Connected` supplies the selected member's client for subsequent actions; wait for live window
data before enabling them. `Offline` retains the last display while the feed retries. The waits
grow through 1, 2, 5, 10 and 30 seconds with jitter. `Reconnect` interrupts an offline wait.
Dropping the command sender stops the task.

The feed resubscribes windows and conversations and reattaches a followed terminal after a
disconnect. Window resync frames request fresh subscriptions. While connected, projection
updates come from the socket; a bounded capability probe every ten seconds detects silent
connections after two misses. Conversation history paging remains the caller's typed client
read using the supplied session ID. Input remains a caller action: `terminal_fence` and
`terminal_screen_fence` use the owner's next sequence and reject a changed incarnation; `detach`
retries a stale fence with a fresh read.

## Snapshot model and cache

`model::Model` and `tree::MissionsTree` hold renderer-independent snapshots compatible with
stui's cache. The crate also preserves the bounded snapshot and event-cursor read APIs for
callers that use the event feed. stui's live screens use collection subscriptions; they run no
event-polling loop. `Model::bootstrap` reads the capability cursor and first collections;
`reload` fills the remaining bounded projections. `sync` consumes events, deduplicates them and
coalesces projection refreshes. Its result is `(changed, invalidated_sessions, cursor_gap)`.
A cursor gap takes a fresh capability cursor, clears the timeline and event deduplication state,
and reloads collections and the mission tree. Callers must discard their other derived state
when the third result is true. This is the event-cursor recovery path; collection streams use
their own resync frames.

`cache::path` scopes an on-disk snapshot to its endpoint and actor under
`$XDG_CACHE_HOME/st3/stui` (or `$HOME/.cache/st3/stui`). The existing version-3 format and location
remain compatible with stui. `save` writes atomically with private directory/file permissions,
omits the conversation timeline, and skips snapshots over 2 MiB. `load` rejects wrong actors,
versions, nonprivate files, future timestamps and snapshots older than seven days. Loaded models
have the status `Cached · refreshing…`. They are display data: actions still require live
snapshots and fresh fences; there is no offline mutation queue.

Run `cargo test --locked -p st3-feed -p stui` to check the extracted feed, cursor-gap recovery,
cache and consumers. Server-backed tests run outside an st seat's `ST_AGENT` environment.

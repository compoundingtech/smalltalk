# Responsive ST3 clients — acceptance criteria

This record extends the active `product-and-idle-recovery` remediation work after live TUI and iOS feedback on 2026-09-24. The typed `st3.client.v0` API is the shared data boundary for CLI, TUI, and iOS. The clients must not parse each other's presentation output or read graph internals.

## Interaction and information architecture

- TUI starts drawing its shell before any daemon read completes. Tab, selection, sidebar, scrolling, and quit keys do not wait on an API call. Target: first frame under 250 ms and local navigation under 100 ms on the development host, with a test server that intentionally delays reads.
- Now, Chat, Control, and Fleet use a clear list/detail hierarchy, breathing room, and boxed sections. A selected row changes the detail pane immediately; asynchronous detail content may then arrive. Use the earlier ST3 TUI experiments as visual reference, without reintroducing fixture-only data.
- Chat shows the selected agent's exact running session when available, recent normalized conversation text, mission/run/current-step context, and a composer. A missing `Agent.current_session_id` must not hide a running session discoverable by owner ID. Undeclared sessions remain in their own section and are read-only.
- Fleet groups by machine and separates people/devices and undeclared local sessions. Control communicates mission progress and current step, not just IDs in a flat text dump.

## Data and cache

- Shared client getters and CLI representations must expose the same underlying facts. Measure representative local daemon reads and address slow projections; do not use async rendering to disguise a slow getter.
- This CLI parity is a standing product requirement, not a new UI-only feature: every datum and operation presented by TUI or iOS has a typed client API and a CLI command that can prove it independently. Session lists and history must have usable bounded pagination; normalized conversation has a CLI follow mode.
- A client paired to any fleet gateway can inspect agents and normalized conversations on every reachable host. Replicated projections are the fast path; bounded host-local transcript reads and live terminal screen/attach use an authenticated, owner-routed daemon relay with exact runtime-incarnation fences. The UI never needs a second pairing or a direct host URL.
- TUI and iOS render the last successful snapshot while refreshing or briefly offline. New selections never show the previous selection's timeline. Cache is read-only; every action still requires online authority and a fresh fence.
- Keep bounded per-session conversation caches (including pagination position, draft, and scroll state) while switching screens or agents. Only the visible conversation holds a live subscription; switching away closes it, and switching back shows cached content immediately while re-establishing the stream. A cursor gap triggers a quiet background resync without discarding visible data.
- iOS persists a bounded projection cache per paired gateway/device so an app restart while offline can show prior data. It must not persist the bearer credential in ordinary storage, print it, or commit personal URLs/IDs. With no cache, show a clear offline empty state.
- TUI may persist a bounded local cache for remote use. It must be scoped to the endpoint and actor, private to the local user, and never treated as action authority.
- Cursor expiry is expected snapshot churn: retry a bounded read from page one, retain old data during retry, and do not display a red error banner for that condition.

## Verification

- Unit/integration tests cover delayed getters with instant key handling, session owner fallback, timeline ordering and stale-response discard, cache restart/offline behavior, and terminal attach/input/detach fences.
- CLI-first connected tests prove cross-host session pagination, normalized conversation follow and resume, owner-routed transcript pages, terminal screen/attach/input/detach, stale-incarnation rejection, cache/reconnect behavior, and no accidental subscription for hidden conversations. The same typed API operations are then exercised by TUI and iOS.
- A live PTY test records cold first-frame and key-to-render latency. A separate measured run records absolute API response times for capabilities, attention, sessions, missions, work, and fleet. A slow getter is fixed or documented with a concrete bound before the UI is considered ready.
- Signed Debug simulator and local TUI are checked against the live paired gateway; physical iPhone reinstallation is not required for every iteration.

## Inbound messaging reliability

- A durable message to a running agent must be surfaced during an active turn as well as while idle. `sent`, `staged`, and transport acceptance are not consumption receipts.
- Delivery is at least once until an exact message-ID consumption receipt is observed. Retries use stable IDs and do not create duplicate visible prompts. A lost receipt must be reconciled from the live session history or transcript so one older head cannot block the entire inbox indefinitely.
- Cover active-turn delivery, crash/restart recovery, lost receipt reconciliation, and FIFO progress with connected regression tests. Do not report this as live-fixed until the wrapper is deployed and an actual active-turn PING is observed end to end.

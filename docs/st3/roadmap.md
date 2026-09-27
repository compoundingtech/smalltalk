# st3 product roadmap

This file records accepted future work. Active fleet work belongs in st3 missions.

## Agent continuity

- [ ] Freeze-dry an idle harness session at a long graph wait or human gate.
- [ ] Rehydrate the exact session when the graph makes work ready again.
- [ ] Define safe suspension points, stored state, timing, and recovery failure behavior.
- [x] Add a harness-neutral session view for SwiftUI, React, and other native clients.
- [x] Include ordered messages, roles, tool activity, status, errors, usage, and incremental updates.
- [x] Discover saved and running Codex, Claude, Pi, OMP, and OpenCode sessions, render their normalized
  conversations read-only, and import an exact session through a fenced `session.import` action.

## User interfaces

- [ ] Hold a product design session for `stui` and the Small Talk mobile application.
- [x] Prepare the [design-session brief](app-design-session-brief.md), including Expo delivery,
  autonomous app-update missions, Tailscale day-one access, and the Fabric-on-iOS spike.
- [x] Define the shared v0 data, actions, live-update, transport, authentication, and online-only
  behavior in the client-v0 contract.
- [ ] Build structured sitrep data from graph queries and exec missions instead of a permanent Markdown report.

UI implementation begins only after the remaining non-UI foundation is landed and verified. The
design session follows these delivery gates in order:

1. **Ship immediately.** Produce the smallest TUI and iOS shells that build, run, install, update,
   roll back, and pass a smoke check. “Hello world” is sufficient at this gate. The release design
   must make a newly accepted build available on its target devices within minutes rather than
   making feature work wait on packaging.
2. **Shape one product on two native surfaces.** Design the core information architecture together,
   confirm every required field and action against client v0, then implement the basic experience
   in both apps. The screen inventory includes Now, missions and work, attention, launch and its
   native Codex conversation, machines and agent tree, normalized conversations, native-session
   import, terminals, devices, and settings.
3. **Prove the network, not a mock.** Exercise the CLI, TUI, and a physical iOS device against st3
   on another machine. Prove discovery, pairing/revocation, terminal attach and control, input,
   resize, reconnect, daemon restart, cursor resync, and clean terminal restoration. Fabric is the
   preferred carrier and Tailscale is supported; Fabric-on-iOS may require SDK research and on-device
   iteration. When Fabric carries an st3 network, both apps expose its effective configuration,
   reachability, and topology rather than hiding transport state.

Cross-machine terminal access belongs to this UI program. It must use the paired client gateway and
terminal protocol; installing st3 on a roaming Mac must not require nesting an interactive
`fabric shell` merely to discover or attach to remote st3 terminals.

## Runtime containment

- [ ] Add optional CPU, memory, process, and disk limits for each mission-owned runtime.
- [x] Put each Linux runtime in its own sibling systemd user scope.
- [x] Put each macOS runtime in its own detached process session.
- [x] Keep the main daemon and replication worker in separate service boundaries.
- [x] Do not lower interactive agent priority through the launchd or systemd service definition.
- [x] Load each new runtime's exported environment from the current default shell startup.
- [x] Keep a captured or guessed PATH out of the service definition.
- [x] Add a macOS privacy and developer permission walkthrough.
- [ ] Evaluate the macOS sandbox and resource-limit surfaces before selecting an enforcement contract.

## CLI reliability

- [x] Fix Unix-socket PTY attach so the client performs a complete WebSocket handshake.
- [x] Use the exported Rust PTY client for raw mode, resize, detach, and terminal cleanup.
- [x] Print readable structured output by default and reserve JSON for `--json`.
- [x] Exercise each CLI parser and help path in an automated test.
- [x] Exercise HTTP and Unix request and WebSocket transports in automated tests.
- [x] Exercise typing, resize, Kitty Shift+Enter, Kitty Ctrl+\ detach, terminal reset, and session survival in a real pseudo-terminal.
- [x] Ship the verified attach fix to each active st3 host.

## Harness accounts

- [ ] Design quota-aware account pools for Codex and OMP harnesses.
- [ ] Keep each conversation sticky to its selected account.
- [ ] Select an eligible account from current usage and health information.
- [ ] Support round-robin or threshold-based selection without moving active conversations.
- [ ] Keep credentials local and make account failure isolate to one account.
- [ ] Evaluate [Comradex](https://github.com/nicosuave/comradex) as a reference for Codex account routing.

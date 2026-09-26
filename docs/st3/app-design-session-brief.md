# TUI and Expo iOS design-session brief

This brief prepares the product design session that follows the CLI walkthrough. It does not start
UI implementation. The goal is one Smalltalk product on two surfaces: a Rust TUI and an Expo iOS
app, both acting as online thin clients of the same `st3.client.v0` API.

## Product promise

From either app Nathan can:

1. understand what work is happening across the Smalltalk network;
2. see and resolve what needs his attention;
3. have a native back-and-forth conversation with Codex to shape a launch;
4. approve and launch durable missions onto eligible machines;
5. read normalized Codex, Claude, Pi, OMP, and OpenCode conversations and import native sessions;
6. inspect or control terminals across machines without nesting a Fabric shell;
7. see connection, pairing, Fabric/Tailscale, topology, update, and recovery state; and
8. ask an agent to change either app, follow that mission, and receive the accepted update on his
   devices through the appropriate release path.

“Chat with Codex” does not mean embedding an untracked local chatbot in each UI. The apps render a
durable normalized st3 conversation and send typed actions to the server. Codex runs in a
mission-owned harness, turns the conversation into typed launch variants, and only graph actions
approve, launch, or deploy work. Conversation text alone grants no authority.

## What is already implemented

The foundation is materially ready for UI work:

- `st3.client.v0` exposes bounded capabilities, snapshots, pagination, details, event resumption,
  cursor-gap recovery, and fenced idempotent actions.
- The API has product resources for attention, messages, launches and decisions, missions, work,
  agents/runtimes, operations/history, and normalized sessions/timelines.
- Launch projection includes typed graph, timeline, assignment, diff, risk, question, validation,
  and live execution overlay data; neither app needs to parse KDL or Markdown to visualize a plan.
- Device-to-person pairing produces scoped, revocable credentials. Remote request bodies cannot
  choose their own actor.
- The terminal protocol provides a screen snapshot, ordered frames, attach/detach, input, resize,
  reconnect, and incarnation fencing over WebSocket.
- A generated Rust client supports local Unix sockets and authenticated remote HTTP. A generated
  Swift package supports the remote contract.
- Saved/running Codex, Claude, Pi, OMP, and OpenCode sessions have a harness-neutral normalized timeline and a
  fenced import action.

The Expo choice uses a generated TypeScript client and models from the same normative
schema/operation manifest as Rust and Swift. Bridging the Swift package merely to perform JSON HTTP
calls would add complexity and create an unnatural React Native boundary. The Fabric transport
itself can remain a future native Expo module that presents a local or virtual loopback URL to the
TypeScript client.

## Delivery gate 1: installable shells immediately

The first implementation mission should end with two deliberately small, real artifacts:

- `stui`: a Rust binary that opens, renders build/endpoint information and “Hello, Smalltalk”,
  restores the terminal exactly, and can be installed or rolled back from a versioned release.
- `apps/ios`: an Expo development/internal-distribution build on Nathan's physical iPhone that
  opens, renders the same build/endpoint information and “Hello, Smalltalk”, has crash reporting,
  and can receive a compatible EAS Update.

The shell gate also creates the release machinery before feature work:

- deterministic checks and smoke tests on every change;
- preview artifacts from an exact commit;
- a human-readable release record with commit, contract version, native runtime, artifact digest,
  target channel/device set, status, and rollback target;
- promotion of the exact tested artifact rather than a rebuild;
- visible update status and last-known-good version in both clients.

For Nathan's own iPhone, an Expo internal-distribution build is the shortest initial path: it is an
ad hoc-signed IPA installed from a URL, but the device UDID must be registered and a new or re-signed
build is needed when the allowed device set changes. TestFlight is the friend-facing path; internal
testers must be App Store Connect users, while external testers can number up to 10,000 and the first
external build requires beta review. Once a compatible binary is installed, EAS Update can deliver
JavaScript/assets without reinstalling the app.

Native changes are a hard boundary. Expo runtime versions bind an update to compatible native code;
adding or changing Fabric, terminal-native code, entitlements, or another native dependency requires
a new iOS build. We should use an explicit runtime version, preview and production channels,
promotion of the same tested bundle, staged rollout where useful, and tested rollback to a prior or
embedded update.

## Delivery gate 2: one product on two surfaces

We should design these screens together and then build vertical slices across both clients:

| Screen | Primary question | Essential actions |
|---|---|---|
| Now | What matters right now? | Open source item, refresh/resync |
| Work | What is active, ready, paused, blocked, or done? | Open mission, work, agent, conversation |
| Mission | What was intended and how is it progressing? | Start/cancel/revise where authorized |
| Attention | What specifically needs me and why? | Approve, reject, resolve/dismiss |
| Launch | What are Codex and I shaping into a mission? | Chat, answer typed questions, compare, preview, approve and launch |
| Plan | What will execute, in what order, where, and with what risk? | Change visualization/lens, open source, inspect diff |
| Agents | Who is working under which mission? | Open agent, runtime, work, conversation, terminal |
| Machines | Where can work run and what is reachable? | Inspect capacity, connection, diagnostics |
| Conversations | What did a person or harness say/do? | Reply, mark read, archive, open tools/usage |
| Native sessions | Which Codex/Claude/Pi/OMP/OpenCode sessions exist outside st3? | Read timeline, inspect fence, import |
| Terminals | Which sessions can I view or control? | Peek, attach, detach, input, resize |
| Devices | Which clients can act as me? | Pair, inspect scopes, revoke |
| Network | How is this client reaching the fleet? | Inspect Fabric/Tailscale config, topology, reachability, retry |
| Releases | What version is installed or being delivered? | Preview status, promote if authorized, rollback |
| Settings | Which network/person/device/update policy is active? | Change non-authoritative preferences |

The TUI and iOS app should share nouns, status colors/labels, empty/error states, navigation targets,
and action semantics. They should not be pixel-identical. Each uses its platform's strengths: dense
keyboard navigation and split panes in the TUI; touch, native sheets, notifications, and compact
drill-down on iOS.

Plan visualization should be designed as lenses over `st3.visualization.v0`, not as one diagram:

- dependency graph for execution structure;
- ordered outline for goals and constraints;
- timeline for queues, gates, loops, retries, and finally work;
- assignment/swimlane view for machines and agents;
- structured revision diff;
- live overlay for state, attempt, claimant, lease, timing, blocker, attention, error, and usage.

## Delivery gate 3: prove remote operation

The same scenario must work from CLI, TUI on Bluey, and a physical iPhone against a different
machine: discover and pair, read Now, hold a launch conversation, approve and start a mission,
follow events, open the resulting normalized harness timeline, attach to a terminal, type and resize,
detach cleanly, reconnect after network movement and daemon restart, recover from a cursor gap, and
revoke the device.

### Immediate carrier: Tailscale

The existing paired-only `st3-client.sock` can be exposed privately with Tailscale Serve. Serve
routes a tailnet HTTPS name to a local service while tailnet access controls still apply. The iPhone
can therefore use its normal Tailscale client and the Expo app can speak ordinary HTTPS/WebSocket to
the paired st3 gateway. This is the fastest way to prove the whole product and remains a supported
fallback.

### Preferred carrier: Fabric

Desktop TUI support composes with Fabric today: `fabric dial` can present the remote paired gateway
locally and the generated Rust client speaks its normal HTTP/WebSocket protocol. The UI should show
the effective Fabric peer, service, route, reachability, latency/path when known, and actionable
failure—not merely “offline”.

Fabric is currently a macOS/Linux CLI/daemon binary, not an iOS library or Swift/Expo module. Some
dependencies have iOS support, but that does not make the application crate embeddable. The iOS
work therefore needs an explicit spike:

1. split a UI-independent Fabric client core from CLI, daemon, PTY, service-manager, and updater
   code;
2. cross-compile that core and its iroh/network dependencies for device and simulator;
3. expose the minimum dial/status/topology API through a stable Rust-to-Swift boundary and an Expo
   local module;
4. decide whether app-scoped direct connections are sufficient (preferred) or a Network Extension
   is genuinely required;
5. preserve keys in Keychain, handle app foreground/background transitions, roaming, relay/direct
   changes, cancellation, and reconnect;
6. build an XCFramework, development build, and physical-device test before promising Fabric as the
   default iOS carrier.

Do not begin with a packet tunnel. An app-scoped Fabric connection to the st3 service is a smaller
authority and product surface. A packet tunnel or app proxy introduces an extension target and
Network Extension entitlement and should be selected only if direct embedded dialing cannot meet
the product need. iOS background execution is scheduled and bounded; st3 must not require the app
to keep a permanent user-space daemon alive while closed. Push notifications can tell the user
about new attention or completed work, and the app can reconnect/resync when opened.

## The autonomous change-to-device loop

The app itself can launch the work that changes it because authority and execution stay server-side:

```text
Nathan chats in TUI/iOS
  -> durable launch conversation and typed candidate
  -> approve-and-launch action
  -> mission runs on an eligible development machine
  -> tests and exact-commit preview release
  -> smoke/device evidence recorded in the mission
  -> deployment policy promotes or asks Nathan in Attention
  -> TUI release or compatible EAS Update reaches the device
  -> client reports installed version and health
  -> automatic rollback or explicit recovery remains available
```

There are two deployment classes:

| Change | Automated delivery | Constraint |
|---|---|---|
| TUI code | Build signed/checksummed binaries, publish exact artifact, update/restart via a small launcher | Never replace a running binary without a verified rollback target |
| Expo JS/assets | Test, publish to preview, smoke, promote exact update to production | Must match installed native runtime |
| Expo native/Fabric/entitlements | EAS/Xcode build, sign, install via internal distribution or TestFlight | Cannot be delivered as an EAS JS update |

EAS Workflows can be triggered from GitHub or its API and can build, test, submit, and publish
updates. We should still model each release as an st3 operation with exact commit/artifact evidence,
idempotency, progress, terminal outcome, and rollback target. EAS is an executor, not the source of
Smalltalk's operational truth.

For the first iteration, agents may autonomously publish preview TUI artifacts and preview EAS
updates after tests. Production promotion should be one explicit typed policy decision in the design
session: always gated, auto-promote after executable acceptance, or risk-based. Native iOS release
will remain constrained by Apple signing, processing, beta review, and device installation even when
the build workflow is automated.

## Decisions for the design session

These choices materially change implementation and should be captured as typed launch decisions,
each with an optional explanation:

1. What are the initial TUI framework and layout conventions?
2. What are the iOS navigation model, visual language, and minimum supported iOS version?
3. Where does the Expo app live, and what package/workspace tooling do both apps share?
4. Do we install Nathan's first device through an internal ad hoc build, TestFlight internal
   testing, or both?
5. Which changes may auto-promote to Nathan's production devices after tests, and which require an
   Attention approval?
6. Is Tailscale the day-one iOS carrier while Fabric embedding proceeds, or must Fabric block the
   first connected release?
7. What notification events deserve APNs rather than appearing only when the online app opens?
8. What telemetry, crash reporting, privacy, and redaction policy is acceptable for both apps?
9. What is the friend onboarding path for Smalltalk, Fabric/Tailscale, device pairing, TestFlight,
   and recovery?

## Recommended first three missions

1. **Ship the shells and release loop.** Create the TUI and Expo app, generated TypeScript client,
   checks, exact-commit preview artifacts, install Nathan's devices, EAS Update channels, release
   projection, and rollback proof. No broad UI.
2. **Build the shared product vertical slices.** Design and implement Now, attention, launch/chat,
   plan visualization, work/mission, agents/machines, conversations/import, terminals, devices,
   network, and release status across both surfaces.
3. **Prove every carrier and machine boundary.** Run the physical cross-machine matrix first over
   Tailscale and desktop Fabric, complete the embedded Fabric iOS spike, then either graduate it or
   retain Tailscale as the explicit supported carrier while recording the exact blocker.

## Primary research

- [Expo: EAS Update introduction](https://docs.expo.dev/eas-update/introduction/) explains compatible
  over-the-air JavaScript/asset updates.
- [Expo: runtime versions](https://docs.expo.dev/eas-update/runtime-versions/) defines the native
  binary/update compatibility boundary.
- [Expo: internal distribution](https://docs.expo.dev/build/internal-distribution/) documents ad hoc
  iOS device registration and installable build URLs.
- [Expo: deployment](https://docs.expo.dev/eas-update/deployment/) documents preview/staging/
  production channels, exact-update promotion, rollouts, and rollback.
- [Expo: EAS Workflows](https://docs.expo.dev/eas/workflows/introduction/) documents automated build,
  update, submission, and test jobs.
- [Expo Modules API](https://docs.expo.dev/modules/get-started/) is the supported Swift bridge for a
  local native Fabric module; native changes require rebuilding the development client.
- [Apple: TestFlight overview](https://developer.apple.com/help/app-store-connect/test-a-beta-version/testflight-overview/)
  documents internal/external testers and first-build external review.
- [Apple: Background Tasks](https://developer.apple.com/documentation/BackgroundTasks) establishes
  that background work is scheduled and bounded rather than an always-running app daemon.
- [Apple: Network Extension entitlement](https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.developer.networking.networkextension)
  shows the additional capability needed only if Fabric becomes a system tunnel/proxy.
- [Tailscale Serve](https://tailscale.com/docs/features/tailscale-serve) documents private tailnet
  HTTPS routing and access-control behavior.

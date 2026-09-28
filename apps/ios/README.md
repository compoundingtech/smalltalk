# Smalltalk iOS

The Expo app implements the Small Talk client contract in [docs/clients/ui-contract.md](../../docs/clients/ui-contract.md), the same one stui implements in the terminal: five tabs (Home, Agents, Missions, Fleet, and a Worktrees tab labelled **demo**), a card for every kind of attention with **Chat about this** and **Go to**, sheets for every name on screen, agent conversations rendered like pi with a details sheet, and missions with their decision embedded, a "what you can do" card, expanding steps and a Groups/Tree switch. Agents with a live terminal open it from a **Terminal** button (keys as buttons, a confirmed Ctrl-C), Missions has **New mission**, Fleet lists your paired devices with a confirmed revoke, and Home and agent rows have swipe actions. Loading, empty and failed are three different states on every list.

## What is shared with stui

`fixtures/clients` is the contract both clients test against. These modules are the TypeScript twins of `crates/stui/src/ui`, and `contract.test.mjs` fails when they drift from the fixtures:

| Module | stui | Pinned by |
|---|---|---|
| `clientView.ts` | `ui::view` (the view model, and a strict reader for its JSON) | `demo-world.json` |
| `words.ts` | mission words, agent states, tiers, tabs, glyphs | `words.json` |
| `theme.ts` | `ui::theme` colour tokens | `theme.json` |
| `messageText.ts`, `harnessConversation.ts` | `clean_message_text`, `adapt::conversation` | `transcripts/*.expected.json` |
| `worldAdapter.ts` | `ui::adapt` (live graph → view model) | `viewModel.test.mjs` |
| `screenModel.ts` | grouping, order, trees, flows and chat targets from `ui::screens` | `viewModel.test.mjs` |

Screens draw only the view model (`World`); they never see st3 client types. `demoStore.ts` and `liveStore.ts` both produce one and carry out the screens' actions.

## Demo mode

**Explore the demo** on the connect screen (or **Leave the demo** in settings) switches to the invented fleet in `fixtures/clients/demo-world.json`, the same one `stui demo` shows. Every button works on the device and nothing is sent anywhere.

## Live mode

The app uses the generated `st3.client.v0` TypeScript client. It connects only to a paired gateway: over HTTPS; over plain HTTP to a Tailscale IPv4 address (100.64.0.0/10), which Tailscale encrypts; or over plain HTTP on the local network to a `*.local` name or an RFC1918 address (10/8, 172.16/12, 192.168/16). LAN HTTP is not encrypted: anyone on that network can read the paired credential and data, and the app says so. App Transport Security allows HTTP only for those ranges and `.local`; any other HTTP gateway URL is rejected. The gateway URL stays in local app preferences; the paired bearer credential stays in iOS Keychain through `expo-secure-store`. No private endpoint or credential is built into the app.

1. On a trusted st3 machine, begin device pairing for the intended person with `st3 devices --as person/... pair --full-control "iPhone"` so this device can answer reviews, send messages and cancel runs. Without `--full-control`, pairing grants a limited read/attention/launch scope set.
2. On the iPhone, enter the **paired-only gateway** URL, then the pairing ID and code. Never publish the privileged `st3.sock`.

A paired device's session actor is `person/…/session/…`; the app takes the person from the device record, so attention addressed to the person shows up. The terminal reuses `terminalControls.ts`: it finds the agent's terminal in the bounded runtime list or by fetching each of the agent's runtimes, follows the screen stream (never polling), says so when no screen arrives within 15 seconds or the terminal restarts, takes a fresh terminal fence for every key, and detaches when the view closes or the app leaves the foreground.

Every action reads a fresh snapshot first and retries once when the graph moved (`stale-fence`). A sent message shows at once, dim and "sending…", keeps the `message/…` id st returns, and gives way to the real message when it appears; a failed send stays red with the reason. After a send the conversation refreshes every few seconds for two minutes; otherwise it refreshes when a graph event names it. Lists reload on graph events at most once per ten seconds. Nothing follows or polls the gateway while the app is in the background; it reads once on return. What st does not let a client do yet (retry a step, restart an agent, approve a revision, answer a feedback gate) is said as such, with the CLI command to use.

## Verifying taps

`maestro/` holds [Maestro](https://maestro.mobile.dev) flows that drive the demo on a simulator: every tab, a card's two-tap approve, Chat about this, a reference sheet and Go to, the Groups/Tree switch, swipe actions, the terminal's confirmed Ctrl-C, New mission and a device revoke. Maestro 2.10 works with Xcode 27 (xcodebuildmcp's UI automation does not: it cannot load SimulatorKit). With a Debug build installed and Metro running, run `maestro --device SIMULATOR_ID test maestro`. Maestro needs a Java runtime (`JAVA_HOME=/opt/homebrew/opt/openjdk` with Homebrew's openjdk); if Homebrew cannot install Maestro because the Command Line Tools are older than Xcode, unzip its release (`maestro.zip`) and run `maestro/bin/maestro`. Keep the simulator on the system keyboard: a third-party keyboard extension swallows typed text.

## Build locally

From this directory run `npm ci`, `npm run typecheck` and `npm test`, then `npx expo prebuild --platform ios --clean --no-install` and `pod install` in `ios/` (with a UTF-8 locale). Build the Debug scheme of `ios/SmalltalkStarter.xcworkspace` for an iOS Simulator and run `npm run start` for Metro; `EXPO_PUBLIC_ST_DEMO=1 npm run start` opens straight into demo mode. Keep normal simulator code signing enabled: building with `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain, so device pairing fails.

For an offline device build, run `npm run export:ios` and build Release with local Apple Development signing and provisioning for that device. The Release bundle is embedded and runs without Metro. No Expo account, EAS service, App Store, or Shareup signing is part of this path.

Debug builds accept links for headless simulator checks; Release builds register none of them:

- `com.compoundingtech.smalltalk.starter://demo` switches to demo mode.
- `com.compoundingtech.smalltalk.starter://pair?gateway=...&id=...&code=...` pairs. Treat it as a temporary credential and do not commit or log its populated form.
- `…://home`, `…://agents`, `…://missions`, `…://fleet`, `…://worktrees` open a tab; `…://attention?id=attention/...`, `…://agent?id=agent/...`, `…://mission?id=mission/...`, `…://machine?name=...`, `…://worktree?id=host:path` open a screen; `…://terminal?id=agent/...` opens an agent's terminal, `…://new-mission` the New mission form, `…://agents?view=tree` and `…://missions?view=tree` the tree views; `…://details?id=agent/...` and `…://peek?subject=...` open a sheet. They carry no authorization.

Generated `ios/`, build output, signing material, local configuration, and proof screenshots are ignored by Git. Do not commit Apple team/device IDs, credentials, machine paths, or private network addresses. Expo SDK 57 needs the `expo-build-properties` scene-lifecycle opt-in for Xcode 27/iOS 27.

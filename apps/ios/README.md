# Smalltalk iOS

The four-tab Expo app uses the generated `st3.client.v0` TypeScript client. Its tabs match stui's: Home, Agents, Missions, and Fleet. It connects only to a paired gateway: over HTTPS; over plain HTTP to a Tailscale IPv4 address (100.64.0.0/10), which Tailscale encrypts; or over plain HTTP on the local network to a `*.local` name or an RFC1918 address (10/8, 172.16/12, 192.168/16). LAN HTTP is not encrypted: anyone on that network can read the paired credential and data, and the app says so. App Transport Security allows HTTP only for those ranges and `.local`; any other HTTP gateway URL is rejected. The gateway URL and tab order stay in local app preferences; the paired bearer credential stays in iOS Keychain through `expo-secure-store`. No private endpoint or credential is built into the app.

## Connect

1. On a trusted st3 machine, begin device pairing for the intended person with `st3 devices --as person/... pair --full-control "iPhone"` if this trusted device should send messages and use mission/work, runtime, and terminal controls. Without `--full-control`, pairing intentionally grants a limited read/attention/launch scope set; an existing limited device must be re-paired to gain controls.
2. On the iPhone, connect Tailscale, enter the **paired-only gateway** URL (HTTPS; `http://100.x.y.z:port` for a paired-only listener bound to the host's Tailscale address; or `http://host.local:port` / a private LAN address for one bound to its LAN address), then enter the pairing ID and code. Never publish the privileged `st3.sock`.
3. Open Home, Agents, Missions, and Fleet. The app shows offline/reconnect state; actions require a live connection. It does not queue offline mutations.

The app holds one WebSocket to the gateway, the collections socket (`docs/st3/client-v0/collections.md`). It subscribes to three windows (up to 200 each of the person's attention, missions, and agents, as stui does), and st pushes each change to them. Missions carry their runs' steps and agents carry their current and queued steps, so the app joins nothing and reads no work or runtime lists. The same socket carries the open conversation and the open terminal. A dropped socket reconnects after 1, 2, 5, 10, then 30 seconds, and the delay resets once a snapshot arrives. Leaving the foreground closes the socket; returning opens a fresh one and shows the new snapshots first. Nothing is read or sent while nothing changes.

## Layout

The chrome is native and keeps the system look and fonts. The content is React and is drawn like stui: IBM Plex Mono, stui's Catppuccin Mocha palette (`theme.ts`, from `crates/stui/src/ui/theme.rs`), dense rows, dim section rules with counts, and state glyphs.

| Piece | Native component | Package |
| --- | --- | --- |
| Tab bar with SF Symbol icons and the Home badge | `UITabBarController` | `@react-navigation/bottom-tabs/unstable` `createNativeBottomTabNavigator`, on react-native-screens' tabs |
| Each tab's stack, navigation bar, back button, swipe-back | `UINavigationController` | `@react-navigation/native-stack` on react-native-screens |
| Bar buttons: Terminal, New mission (+), and the Missions options menu | `UIBarButtonItem` and `UIMenu` | native-stack `unstable_headerRightItems` / `unstable_headerLeftItems` |
| Agents filter | `UISearchController` | native-stack `headerSearchBarOptions` |
| Agents list/tree switch and the planner choice | `UISegmentedControl` | `@react-native-segmented-control/segmented-control` |
| Long-press menus on Home and Agents rows | `UIContextMenuInteraction` / `UIMenu` | `@react-native-menu/menu` |
| Pull to refresh | `UIRefreshControl` | React Native `RefreshControl` |
| Confirmations and tab-order moves | `UIAlertController` alerts and action sheets | React Native `Alert` and `ActionSheetIOS` |
| New mission | modal sheet | native-stack `presentation: 'modal'` |

Each tab holds its own native stack, and every detail (a conversation, terminal, mission, attention item, launch, past sessions) can be pushed in any tab, so the back button returns where the person came from. A conversation and a terminal hide the tab bar while they are on top, as Messages does.

- **Home** lists what stui's Home lists: every unresolved attention item for the paired person, unread messages included, grouped by tier (somebody is stopped on you, something broke, today, when there is time) with stui's glyphs and a legend. The tab's badge counts them. An item opens its detail, with resolve when st offers it; a long press offers the agent's conversation and the mission.
- **Agents** groups and sorts seats as stui does (waiting on you, broken, working, idle, stopped), with sessions st found running but did not start last. Each row has the state glyph, name, harness, age, and graph path. The segmented control switches to stui's tree of agent paths. The search field filters by name, path, harness, or host. An agent opens its conversation: the harness transcript, tool calls (collapsed to their last five lines; tap to expand), Small Talk with a bar, and st's events on one dim line. Paired delivery pauses and recoveries fold into one quiet line. The list is inverted and pinned to the newest entry above the `›` composer unless the person scrolls back; `↓ latest` returns. A sent message shows as `you · sending…` until st has it. The Terminal bar button opens the agent's live terminal; a person with `terminal.input` can type a line or send Enter, Tab, Esc, Up, Down, and a confirmed Ctrl-C. Each input fetches a fresh terminal fence and refuses a changed runtime incarnation.
- **Missions** gives each mission stui's one word for who has to move (needs you, stalled, unstaffed, unclaimed, queued, working, watching, held, idle, done, failed, cancelled), in that order, with a progress meter. System missions (st's loop rounds and CI) are hidden until the options menu shows them. A mission shows its steps as a pipeline and who holds each; open launches are listed below, with variant preview, approval, and revision; + creates a launch.
- **Fleet** shows machines, sessions found running on the gateway machine, agent work, this connection, paired devices, and the tab order.
- **Spaces** (st keeps them as `glass` resources) have a fifth tab, on unless turned off under Fleet › tabs. It holds the person's spaces from stui, followed live from st when the gateway grants `glasses`. The tab shows one thing at a time: a picker when there is more than one space, then the chosen space's tabs, Home first, with `◆` where something needs the person. A tab with one pane opens it (a conversation, a mission, or Fleet for a machine); a split tab lists its panes, and each opens the same way. A pane whose subject st no longer lists says it is gone. The phone reads spaces and never changes them.

Pure logic lives in tested modules (`agentsView.ts`, `homeView.ts`, `missionsView.ts`, `conversationView.ts`, `markdown.ts`, `tabs.ts`), each a port of the matching stui code. A transcript entry the app does not understand is shown as a dim event line, never dropped silently, and never breaks the rest of the conversation. Launches, machines, devices, and sessions have no window: each loads (at most 30 items a page) when the screen that shows it opens, on pull to refresh, and after an action there. A collection that fails to load is named in a banner instead of appearing empty.

## Build locally

For a step-by-step setup, see [build and run the iOS app](../../docs/ios-app.md), including prerequisites, simulator and device builds, and pairing.

From this directory run `npm ci`, `npm run typecheck`, `npm test`, then `npx expo prebuild --platform ios --clean --no-install`. Install CocoaPods, run `npm run pods` (the wrapper sets `LANG` and `LC_ALL` to `en_US.UTF-8`), and open the generated `ios/smalltalk.xcworkspace` in Xcode. For daily development, build the Debug scheme for an iOS Simulator and run `npm run start` for Metro. Keep normal simulator code signing enabled: building with `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain, so device pairing fails. A physical device is optional for this development proof.

For an offline device build, run `npm run export:ios` and build Release with local Apple Development signing and provisioning for that device. The Release bundle is embedded and runs without Metro. No Expo account, EAS service, App Store, or Shareup signing is part of this path.

The Debug app accepts a short-lived pairing deep link for headless simulator checks: `com.compoundingtech.smalltalk.starter://pair?gateway=...&id=...&code=...`. The handler is disabled in Release. Treat the link as a temporary credential and do not commit or log its populated form.

For connected Debug smoke tests, `com.compoundingtech.smalltalk.starter://tab/Fleet` opens a tab (the earlier names Now, Chat, and Control still work), `com.compoundingtech.smalltalk.starter://mission?id=mission/...` opens a mission, `com.compoundingtech.smalltalk.starter://agent?id=agent/...` opens an agent's conversation, and `com.compoundingtech.smalltalk.starter://session?id=session/...` opens an exact conversation. Add `&terminal=terminal/...` to the session link to inspect its live terminal screen. `com.compoundingtech.smalltalk.starter://tree?on=1` switches Agents to the tree, and `com.compoundingtech.smalltalk.starter://scroll?y=800` scrolls the visible list for screenshots. These links are disabled in Release and carry no authorization: the already-paired client still has to pass the gateway's normal checks.

The simulator asks before it opens each link it is handed, so a headless run can instead put links in `apps/ios/.env` for Metro to inline into the Debug bundle: `EXPO_PUBLIC_ST3_TEST_PAIR_LINK` pairs at launch, `EXPO_PUBLIC_ST3_TEST_TAB` picks the first tab, and `EXPO_PUBLIC_ST3_TEST_LINKS` (space-separated) follows each link six seconds apart. Restart Metro with `--clear` after changing them.

For screenshots without a real st, `node demoGateway.mjs 8791` serves invented data: attention, missions, agents, one conversation, and one terminal. Pair a Debug simulator with `com.compoundingtech.smalltalk.starter://pair?gateway=http://<the Mac's LAN or Tailscale IPv4>:8791&id=demo&code=demo`. It is not authenticated; never point a Release build or a device at it.

Generated `ios/`, build output, signing material, local configuration, and proof screenshots are ignored by Git. Do not commit Apple team/device IDs, credentials, machine paths, or private network addresses. Expo SDK 57 needs the `expo-build-properties` scene-lifecycle opt-in for Xcode 27/iOS 27.

An opt-in Debug fabric carrier has its own [build and isolated proof instructions](modules/st-fabric/README.md). Default builds do not link it. Its temporary client uses only a native-created loopback listener and leaves the saved Tailscale or LAN gateway unchanged.

# Smalltalk iOS

The four-tab Expo app uses the generated `st3.client.v0` TypeScript client. Its tabs match stui's: Home, Agents, Missions, and Fleet. It connects only to a paired gateway: over HTTPS; over plain HTTP to a Tailscale IPv4 address (100.64.0.0/10), which Tailscale encrypts; or over plain HTTP on the local network to a `*.local` name or an RFC1918 address (10/8, 172.16/12, 192.168/16). LAN HTTP is not encrypted: anyone on that network can read the paired credential and data, and the app says so. App Transport Security allows HTTP only for those ranges and `.local`; any other HTTP gateway URL is rejected. The gateway URL and tab order stay in local app preferences; the paired bearer credential stays in iOS Keychain through `expo-secure-store`. No private endpoint or credential is built into the app.

## Connect

1. On a trusted st3 machine, begin device pairing for the intended person with `st3 devices --as person/... pair --full-control "iPhone"` if this trusted device should send messages and use mission/work, runtime, and terminal controls. Without `--full-control`, pairing intentionally grants a limited read/attention/launch scope set; an existing limited device must be re-paired to gain controls.
2. On the iPhone, connect Tailscale, enter the **paired-only gateway** URL (HTTPS; `http://100.x.y.z:port` for a paired-only listener bound to the host's Tailscale address; or `http://host.local:port` / a private LAN address for one bound to its LAN address), then enter the pairing ID and code, plus the person-root fingerprint copied separately from the trusted machine. A missing fingerprint is refused unless you deliberately choose the warned unpinned override. Never publish the privileged `st3.sock`.
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

New agent offers a repository from the selected machine’s agents, or an absolute path you type. Selecting a repository creates a Git worktree using the agent’s safe simple name as its default branch, with `origin/main` as the editable base. An empty repository uses a plain workspace. Workspace is optional in either case; st chooses a new directory when it is empty. The conversation header shows the requested worktree and branch, and any checkout failure remains visible as the agent’s fault.

Conversation and session views, Home rows, requests and answers, device-signing data and client naming live in the shared [`@smalltalk/st3-views`](../../clients/typescript/st3-views/README.md) package. The phone imports it through a file dependency; its tests check the same transcript and signing fixtures as Rust. Other pure phone logic lives in tested modules (`agentsView.ts`, `missionsView.ts`, `markdown.ts`, `tabs.ts`). A transcript entry the app does not understand is shown as a dim event line, never dropped silently, and never breaks the rest of the conversation. Launches, machines, devices, and sessions have no window: each loads (at most 30 items a page) when the screen that shows it opens, on pull to refresh, and after an action there. A collection that fails to load is named in a banner instead of appearing empty.

## Build locally

For a step-by-step setup, see [build and run the iOS app](../../docs/ios-app.md), including prerequisites, simulator and device builds, and pairing.

First install the shared package's locked dependencies from the repository root: `npm ci --prefix clients/typescript/st3-views --ignore-scripts --no-audit --no-fund`. npm links this local package; it does not install the linked package's dependencies from the phone directory. Then from this directory run `npm ci`, `npm run typecheck`, `npm test`, and `APP_VARIANT=dev npx expo prebuild --platform ios --clean --no-install`. Install CocoaPods, run `npm run pods` (the wrapper sets `LANG` and `LC_ALL` to `en_US.UTF-8`), and open the generated workspace under `ios/` in Xcode. For development, build the Debug scheme for an iOS Simulator and run `npm run start` for Metro. Keep normal simulator code signing enabled: building with `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain, so device pairing fails. A physical device is optional for this development proof.

For the daily offline device build, prebuild with `APP_VARIANT=daily npx expo prebuild --platform ios --clean --no-install`, install pods, and build Release with local Apple Development signing and provisioning for that device. The Release bundle is embedded and runs without Metro. The plugin records the selected variant in generated `ios/.xcode.env` for both bundle and update-resource build phases; do not override it in `.xcode.env.local`. No Expo account, EAS service, App Store, or Shareup signing is part of this path.

The Debug app accepts a short-lived pairing deep link for headless simulator checks: `com.compoundingtech.smalltalk.starter://pair?gateway=...&id=...&code=...`. The link opens the pairing form with the code prefilled; it does not submit the code until a separately supplied fingerprint or explicit unpinned override is chosen. The handler is disabled in Release. Treat the link as a temporary credential and do not commit or log its populated form.

For connected Debug smoke tests, `com.compoundingtech.smalltalk.starter://tab/Fleet` opens a tab (the earlier names Now, Chat, and Control still work), `com.compoundingtech.smalltalk.starter://mission?id=mission/...` opens a mission, `com.compoundingtech.smalltalk.starter://agent?id=agent/...` opens an agent's conversation, and `com.compoundingtech.smalltalk.starter://session?id=session/...` opens an exact conversation. Add `&terminal=terminal/...` to the session link to inspect its live terminal screen. `com.compoundingtech.smalltalk.starter://tree?on=1` switches Agents to the tree, and `com.compoundingtech.smalltalk.starter://scroll?y=800` scrolls the visible list for screenshots. These links are disabled in Release and carry no authorization: the already-paired client still has to pass the gateway's normal checks.

The simulator asks before it opens each link it is handed, so a headless run can instead put links in `apps/ios/.env` for Metro to inline into the Debug bundle: `EXPO_PUBLIC_ST3_TEST_PAIR_LINK` opens the pairing form at launch, `EXPO_PUBLIC_ST3_TEST_TAB` picks the first tab, and `EXPO_PUBLIC_ST3_TEST_LINKS` (space-separated) follows each link six seconds apart. Restart Metro with `--clear` after changing them.

For screenshots without a real st, `node demoGateway.mjs 8791` serves invented data: attention, missions, agents, one conversation, and one terminal. Pair a Debug simulator with `com.compoundingtech.smalltalk.starter://pair?gateway=http://<the Mac's LAN or Tailscale IPv4>:8791&id=demo&code=demo`. It is not authenticated; never point a Release build or a device at it.

Generated `ios/`, build output, signing material, local configuration, and proof screenshots are ignored by Git. Do not commit Apple team/device IDs, credentials, machine paths, or private network addresses. Expo SDK 57 needs the `expo-build-properties` scene-lifecycle opt-in for Xcode 27/iOS 27.

An opt-in Debug fabric carrier has its own [build and isolated proof instructions](modules/st-fabric/README.md). Default builds do not link it. Its temporary client uses only a native-created loopback listener and leaves the saved Tailscale or LAN gateway unchanged.

## Daily signed app updates

`app.config.js` selects `APP_VARIANT=dev` (the default) or `APP_VARIANT=daily`:

| Variant | Installed identity | Bundle identifier | Updates |
| --- | --- | --- | --- |
| `dev` | Smalltalk Dev, amber DEV-badged icon | `com.compoundingtech.smalltalk.starter` | Disabled; Debug/Metro behavior unchanged |
| `daily` | Smalltalk, ordinary icon | `com.compoundingtech.smalltalk` | Embedded/cached signed bundle; paired-gateway OTA |

The two apps coexist and keep separate Keychain profiles. Changing variants requires a clean prebuild, not just changing the Metro environment. The optional EAS simulator profile selects dev; preview selects daily. Daily requires a Release binary: Expo's update APIs do not run in ordinary Debug/Metro builds. Set `ST_IOS_UPDATES_CERT` to your own public certificate path for daily prebuild, builds and export; without it daily configuration fails closed. Dev requires no update certificate. The plugin carries the public path into the ignored generated `.xcode.env` so Xcode's embedded-bundle phase uses the same certificate configuration.

Daily uses Expo SDK 57 / `expo-updates ~57.0.23`, `checkAutomatically: NEVER`, an inert build-time `Authorization: Bearer unavailable` placeholder, and `nativeVersion` runtime selection. The daily runtime is **`0.1.0(2)`**. Increase `ios.buildNumber` in `app.config.js` for **every native dependency, module, configuration, or certificate change**, then rebuild and reinstall. JS/assets-only publications keep the installed runtime; do not override a compatibility mismatch by relabeling an update.

### Authentication and recovery

1. Start immediately from the embedded or previously verified cached bundle. Native startup never anonymously checks the gateway.
2. Await the existing SecureStore/Keychain profile read (or a completed verified pairing). Missing, locked, invalid or revoked credentials leave the current bundle usable offline.
3. On an active app, POST `{app:"com.compoundingtech.smalltalk",channel:"daily"}` to the paired gateway's `/v1/client/app-updates/token`, authenticated by the paired credential and requiring `read.app-updates`. Decode the standard st response's `value: {token,expiresAtUnixMs}`.
4. Set the paired gateway origin through the native transport bridge. Pass **only** the returned 15-minute, update-read-only, app/channel-bound bearer and expiry to `StAppUpdates.setUpdateToken`. The bridge retains them only in locked process memory. Neither the broad paired credential nor the narrow bearer enters Expo configuration, UserDefaults or the cached update row.
5. Use stock `checkForUpdateAsync` / `fetchUpdateAsync`; Expo verifies the RSA-SHA256 signed exact manifest bytes and asset hashes. Immediately before sending a request, the native transport substitutes the paired origin for the inert manifest URL and authorizes manifests and same-origin, app/channel-restricted asset routes. Other destinations and redirects are refused. Clear the in-memory token after the check/download. Tokens expire, are invalidated by parent pairing revocation or daemon restart, and can be explicitly revoked through the paired-only token-revocation endpoint.
6. Downloading **never automatically reloads active work**. An explicit “Restart now” consent applies it only while foregrounded and still paired to that gateway. “Next launch” keeps the current session untouched. A download that completes in the background offers consent on the next foreground; a normal process restart selects the cached verified update. Failed authentication, incompatible runtimes, invalid signatures and download failures keep the current verified bundle.

SDK 57's [stock URL override](https://docs.expo.dev/eas-update/override/) requires `disableAntiBrickingMeasures` and is intended for previews. **We do not use it or the stock header override.** Its launcher compares the exact configured URL and entire request-header dictionary against each cached row, independently of manifest filters. `plugins/with-paired-app-updates.js` compiles `plugins/native/StPairedGateway.swift` into a source-built EXUpdates pod and inserts a guarded transport hook in `FileDownloader`. Only the outgoing `URLRequest` changes: Expo's inert URL, non-secret headers, fixed scope, certificate, runtime, stock launch selection and recovery policy remain identical at download, reload and cold launch. A cleared/rotated token or absent gateway cannot make a verified cached row ineligible. Signing/NEVER/embedded recovery remain required. The plugin rejects a non-SDK-57 dependency or a changed transport boundary until reviewed. Do not set `disableAntiBrickingMeasures`, disable embedded updates, or use the preview override to work around pairing.

Source compilation is selected by `expo.autolinking.ios.buildFromSource` in `package.json` and the Podfile source opt-in. The native bridge and JS session accept only a bare gateway origin (no reverse-proxy path prefix); all update routes are fixed root paths.

See [SDK 57 updates](https://docs.expo.dev/versions/v57.0.0/sdk/updates/), [download controls](https://docs.expo.dev/eas-update/download-updates/), and [code signing](https://docs.expo.dev/eas-update/code-signing/).

### Local signing and publication

Every deployer owns their own signing identity. This public repository does **not** ship an operator's verification certificate or private key. `ST_IOS_UPDATES_CERT` is a required daily-build setting and is resolved at prebuild to the deployer's public certificate; missing configuration or an unreadable file fails the build rather than shipping unsigned OTA. The private counterpart belongs only to the publisher's approved credential store (password manager or KMS), never tracked source, st3, a gateway or an export. Record its ownership, location and rotation in that private inventory.

From `apps/ios`, generate your own pair locally using Expo's code-signing utility. Set `SIGNING_DIR` to an approved private directory outside tracked source first; no Expo/EAS account is needed:

```sh
umask 077
mkdir -p "$SIGNING_DIR"
npx expo-updates codesigning:generate \
  --key-output-directory "$SIGNING_DIR" \
  --certificate-output-directory "$SIGNING_DIR" \
  --certificate-validity-duration-years 10 \
  --certificate-common-name "Your Organization"
export ST_IOS_UPDATES_CERT="$SIGNING_DIR/certificate.pem"
APP_VARIANT=daily npx expo prebuild --platform ios --clean --no-install
```

The public certificate is embedded in the native daily binary; the signing key remains outside the repo. Losing or rotating the key or certificate requires incrementing daily `buildNumber`, rebuilding/installing and targeting the new runtime. The gateway preserves exact signed bytes and headers; it never needs the private key and does not perform the device's cryptographic verification. `certs/test-fixture.pem` is a **throwaway configuration-test fixture only** (OpenSSL RSA-2048, CN=Smalltalk Test Fixture Only; its newly generated private key was discarded directly to `/dev/null`). Never select it for a deployed build: there is no retained publishing key for it.

The test fixture is generated, not hand-edited: LibreSSL 3.3.6 on 2026-10-07, RSA-2048 with fresh throwaway entropy, digital-signature/code-signing usage and a 3650-day lifetime. Its SHA-256 is `ef35e28fae28972b20c8cfea0729ab114d45400a3daad2449721c917eec8dd80`. From the repository root, regeneration is:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout /dev/null \
  -out apps/ios/certs/test-fixture.pem -days 3650 \
  -subj '/CN=Smalltalk Test Fixture Only' \
  -addext 'keyUsage=critical,digitalSignature' -addext 'extendedKeyUsage=codeSigning'
```

Regeneration intentionally creates a different throwaway identity: update this fingerprint and never reuse the fixture as a deployed identity.

From `apps/ios`, export and sign for the origin the paired phone actually uses:

```sh
export ST_IOS_UPDATES_CERT="$SIGNING_DIR/certificate.pem"
APP_VARIANT=daily npm run export:ios -- --output-dir dist
APP_VARIANT=daily npx expo config --type public --json > dist/expo-config.json
node scripts/sign-app-update.mjs --dir dist \
  --app com.compoundingtech.smalltalk --channel daily \
  --origin https://gateway.example --runtime-version '0.1.0(2)' \
  --private-key "$SIGNING_DIR/private-key.pem" --expo-config dist/expo-config.json
st app-updates publish --app com.compoundingtech.smalltalk --channel daily --dir dist
```

Replace the example origin and runtime with the paired gateway origin and installed binary runtime. The helper generates `manifest.json` (exact Expo v1 signed bytes), `manifest.signature` (the structured `expo-signature` header, keyid `main`), and `publication.json` (asset hash/content-type/export-relative-path index) beside Expo's exported files. Expo bundler asset keys are preserved; manifest asset hashes are unpadded base64url SHA-256 and authenticated asset URLs use lowercase hex SHA-256. Never modify the manifest after signing. Manifest and assets must use the same authenticated origin; the backend refuses a Host/origin mismatch rather than redirecting credentials.

The CLI publishes only through st's local Unix socket, importing JSON/base64 manifest/signature/assets into immutable durable storage before promoting the channel head. No client-gateway credential, even full-control, may publish. Use `--ref <branch/ref>` for an explicit branch publication and `--expected-head <update-UUID>` as a channel-head fence. Publication retains all published manifest/asset references; the node rejects capacity beyond 2 GiB or 10,000 publications per channel instead of deleting referenced objects.

Local orchestration—not this public app—watches each merge to main, serializes publication and prevents stale jobs overwriting a newer selection. An explicit branch publish is selected until the next main publication. A native mismatch leaves the last compatible update available and emits the deduplicated local `app-update.native-build-required` observation; the local watcher notifies its configured maintainer, who owns Apple signing/build/install access. st3 does not build native binaries.

### On-phone acceptance plan

After repository checks, install both variants with normal signing. Dev must retain Metro/pairing behavior and show its DEV badge; daily must launch its embedded Release bundle with Metro stopped and the gateway unreachable. Pair daily with a device granted `read.app-updates`, publish one correctly signed compatible update and confirm narrow authorization is used for both manifest and asset reads. Decline the reload prompt during a conversation or terminal session: nothing reloads or loses active work; restart explicitly or cold-launch and confirm the new update ID. Background during download: no reload happens, and consent is offered only after foreground.

Exercise missing/locked Keychain credentials, revoked pairing, expired token, daemon restart, offline launch, wrong signing key, altered manifest bytes, altered asset bytes, origin mismatch and a mismatched native runtime. Each must retain a usable embedded/cached bundle without anonymous fallback. Re-pair during an in-flight mint/download and ensure stale credentials cannot authorize requests or trigger consent. After a download, clear authorization, then cold-launch offline and verify the downloaded update ID; repeat with a freshly minted bearer to prove token rotation does not affect cache eligibility. Publish a broken-but-signed JS update and verify Expo recovers to a working cached/embedded bundle with anti-bricking enabled. Change the native certificate/buildNumber, reinstall, and ensure the old runtime cannot displace the new one. These are release-device/native acceptance checks, not claims that Node unit tests prove native recovery.

`appUpdates.unit.test.mjs` covers hydration ordering, narrow-token confinement, expiration/revocation failures, serialization/re-pair fencing, consent/background safety, in-memory token clearing and foreground refresh; `appConfig.unit.test.mjs` covers both variants, recovery/signing defaults and failure without an operator certificate. On macOS, `appUpdatesNative.integration.test.mjs` compiles the real Swift transport and unmodified SDK 57 launcher/manifest-filter sources against data-only model fixtures: it reproduces the old URL/header mismatch, verifies selection after clearing/rotating authorization and cold launch, and checks asset confinement, expiry and redirect rejection. It does not substitute for the on-phone native proof. The DEV icon is generated, not hand-edited: regenerate on macOS with `swift scripts/generate-dev-icon.swift assets/icon.png assets/icon-dev.png`. Its source icon SHA-256 is `267eef3f93327ee1825f2cf480f61c1c466474140b962227b8b44f5aab964aec`; generator inputs are that image, CoreGraphics/ImageIO/CoreText and Helvetica-Bold on macOS, plus the badge geometry/colors in the script.


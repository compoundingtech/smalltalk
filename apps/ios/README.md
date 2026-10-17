# Fractal iOS

The four-tab Expo app uses the generated `st3.client.v0` TypeScript client. Its tabs are Now, Resources, Fleet and Spaces. It connects only to a paired gateway: over HTTPS; over plain HTTP to a Tailscale IPv4 address (100.64.0.0/10), which Tailscale encrypts; or over plain HTTP on the local network to a `*.local` name or an RFC1918 address (10/8, 172.16/12, 192.168/16). LAN HTTP is not encrypted: anyone on that network can read the paired credential and data, and the app says so. App Transport Security allows HTTP only for those ranges and `.local`; any other HTTP gateway URL is rejected. The gateway URL and tab order stay in local app preferences; the paired bearer credential stays in iOS Keychain through `expo-secure-store`. No private endpoint or credential is built into the app.

## Connect

1. On a trusted st3 machine, begin device pairing for the intended person with `st devices pair --as person/ada --full-control "iPhone"` if this trusted device should send messages and use mission/work, runtime, and terminal controls. Without `--full-control`, pairing intentionally grants the limited default projection, glasses, arrangement, terminal-output, attention, and launch scopes; an existing limited device must be re-paired to gain controls.
2. On the iPhone, connect Tailscale, enter the **paired-only gateway** URL (HTTPS; `http://100.x.y.z:port` for a paired-only listener bound to the host's Tailscale address; or `http://host.local:port` / a private LAN address for one bound to its LAN address), then enter the pairing ID and code, plus the person-root fingerprint copied separately from the trusted machine. A missing fingerprint is refused unless you deliberately choose the warned unpinned override. Never publish the privileged `st3.sock`.
3. Open Now, Resources, Fleet and Spaces. The app shows offline/reconnect state; actions require a live connection. It does not queue offline mutations.

The app holds one WebSocket to the gateway, the collections socket (`docs/st3/client-v0/collections.md`). It subscribes to three windows (up to 200 each of the person's attention, missions, and agents, as stui does), and st pushes each change to them. Missions carry their runs' steps and agents carry their current and queued steps, so the app joins nothing and reads no work or runtime lists. The same socket carries the open conversation and the open terminal. A dropped socket reconnects after 1, 2, 5, 10, then 30 seconds, and the delay resets once a snapshot arrives. Leaving the foreground closes the socket; returning opens a fresh one and shows the new snapshots first. The server’s eight-second WebSocket protocol pings are answered natively and reset a twenty-four-second receive deadline without an HTTP capability probe. If frames and pings stop, the phone closes the socket and keeps the existing offline wording and reconnect delays. The autolinked `StCollections` module exposes pings through SocketRocket for both gateway and Fabric-loopback sockets; Expo Go uses its ordinary WebSocket and cannot observe protocol heartbeats. No inventory read is sent while nothing changes.

Observation reporting holds one send slot during both batch preparation and transport. Closing a measured minute can synchronously notify the reporter; that notification cannot prepare a second batch while the first is being formed. A null or empty batch releases the slot without delaying the next measurement.

## Layout

The chrome uses native tabs and stacks. Now and alert detail use system typography, device light/dark appearance and wrapping, inset grouped rows. Alert rows label kind, requester and waiting context; opening an alert never acknowledges it. Updates require explicit Mark read, and closed alerts remain until cleared. Other tabs retain the stui-style IBM Plex Mono and Mocha presentation. Terminal cells, code and identifiers remain monospace.

| Piece | Native component | Package |
| --- | --- | --- |
| Tab bar with SF Symbol icons and the Now badge | `UITabBarController` | `@react-navigation/bottom-tabs/unstable` `createNativeBottomTabNavigator`, on react-native-screens' tabs |
| Each tab's stack, navigation bar, back button, swipe-back | `UINavigationController` | `@react-navigation/native-stack` on react-native-screens |
| Bar buttons: Terminal and New mission (+) | `UIBarButtonItem` and `UIMenu` | native-stack `unstable_headerRightItems` / `unstable_headerLeftItems` |
| Resources filter and searchable agent/mission lists | `UITextField` | React Native `TextInput` and native-stack `headerSearchBarOptions` |
| Planner choice | `UISegmentedControl` | `@react-native-segmented-control/segmented-control` |
| Long-press menus on Now rows | `UIContextMenuInteraction` / `UIMenu` | `@react-native-menu/menu` |
| Pull to refresh | `UIRefreshControl` | React Native `RefreshControl` |
| Confirmations and tab-order moves | `UIAlertController` alerts and action sheets | React Native `Alert` and `ActionSheetIOS` |
| New mission | modal sheet | native-stack `presentation: 'modal'` |

Each tab holds its own native stack, and every detail (a conversation, terminal, mission, attention item, launch, past sessions) can be pushed in any tab, so the back button returns where the person came from. A conversation and a terminal hide the tab bar while they are on top, as Messages does.

- **Now** lists what stui's Home lists: every unresolved request, review, approval, native harness prompt and provider sign-in for the paired person, plus the updates they asked for, grouped by tier (somebody is stopped on you, today, when there is time) with stui's glyphs and a legend. The first thing it prints is the alert count: nothing at zero, `1 alert`, `N alerts` otherwise; the tab's badge shows the same number. An alert is whatever blocks or waits on the person (an update asks nothing and is not one). The app reads `alert` on each attention row, and a daemon that predates alerts is read as everything but an update or an unread message. The wire names (`attention`, `/attention`) are unchanged, so older and newer builds keep working. An item opens its detail, with resolve when st offers it; a long press offers the agent's conversation and the mission.
- **Resources** keeps the Agents and Missions lists one tap away, with their search, list/tree controls, conversation results and mission cards. It opens on the person's own folders, then Everything else grouped into Agents, Missions, Terminals and Machines. The filter stays on top. Agent and mission status glyphs and needs-you dots remain visible, including sessions found running. A row opens its conversation, mission or terminal; long-press offers Move to. New folder, folder rename and confirmed deletion use `arrangement.edit`; each folder's + finds a resource not already in it. Drag handles move resources or reorder rows and folders; tapping a handle changes nothing. The stream supplies the winning registers after edits. Collapse choices and unavailable resource names stay on this phone.
- **Fleet** shows machines, sessions found running on the gateway machine, this connection, paired devices, and the tab order.
- **Alerts in a conversation.** Each alert shows above the conversation of the agent it belongs to (`conversation_id`; a provider sign-in shared by several seats shows in each, once, naming the others), a line that opens to its question and its answers, and starts open when it is the only one. Requests are answered there as they are on Now. A native prompt (`prompt.respond`) shows the pending tool call in full, from the conversation where the harness records it before asking, beside the answers st offers for it: Allow and Deny for a permission or a Codex approval, Approve plan and Deny for a plan, the question's options for a Claude question (any number for a multi-select, sent together with Send, or Decline), and a screen prompt's own answers (Use it, Use all, Skip; Trust, Exit or Quit). The prompt names no call, so Allow and Approve plan are offered only when exactly one call is unanswered and shown; with several, all are listed with a pointer to the terminal. Deny and a screen prompt's answers are always offered, and an answer in the terminal still wins. Now, where the conversation is not loaded, says to open it to see the call. A refusal because the prompt is gone says so in st's words and is not sent again. A prompt st cannot deliver an answer to says to answer it in the terminal. A seat whose harness is compacting its conversation reads `compacting` in the agent strip and the Agents list: a status, never an alert.
- **Spaces** is always one of the four tabs. It lists the person's spaces by name, followed live when the gateway grants `glasses`; selecting a space shows its tabs grouped by pane in the computer's order. A pane opens its conversation, mission, resource, terminal or Fleet for a machine. An unavailable subject stays marked. The phone reads spaces and never changes them. Saved spaces and sidebar folders remain available offline, with the age of the cached data shown.

New agent offers a repository from the selected machine’s agents, or an absolute path you type. Selecting a repository creates a Git worktree using the agent’s safe simple name as its default branch, with `origin/main` as the editable base. An empty repository uses a plain workspace. Workspace is optional in either case; st chooses a new directory when it is empty. The conversation header shows the requested worktree and branch, and any checkout failure remains visible as the agent’s fault.

Conversation and session views, Home rows, requests and answers, device-signing data and client naming live in the shared [`@smalltalk/st3-views`](../../clients/typescript/st3-views/README.md) package. The phone imports it through a `workspace:*` dependency; its tests check the same transcript and signing fixtures as Rust. Other pure phone logic lives in tested modules (`agentsView.ts`, `missionsView.ts`, `markdown.ts`, `tabs.ts`). A transcript entry the app does not understand is shown as a dim event line, never dropped silently, and never breaks the rest of the conversation. Launches retain their explicit reads. When the member grants `inventory-collections` version 1, clients, usage, machines, devices, native sessions and terminal lists follow screen-scoped windows on the same socket, up to 200 rows each. Shared screens share a slot, including each of the three selected usage periods; a window leaves thirty seconds after its last focused screen leaves. At most fifteen total subscriptions fit the sixteen-slot bound. Pull to refresh resubscribes for authoritative snapshots. An older member retains reads on focus and explicit pull, with a Fleet note that inventories are read, not followed; no automatic inventory timer remains. A collection that fails is named instead of appearing empty.

## Build locally

For a step-by-step setup, see [build and run the iOS app](../../docs/ios-app.md), including prerequisites, simulator and device builds, and pairing.

With Node 24 and Corepack, run `corepack enable` and `pnpm install --frozen-lockfile` from the repository root. Corepack provisions the root's pnpm 12.7.0; the single install covers the client, views and iOS packages. Alternatively, `nix develop .#web` supplies pnpm 12.7.0, Node 24.20, Bun 1.4.2 and Buck2. Then from this directory run `pnpm typecheck`, `pnpm test`, and `pnpm exec expo prebuild --platform ios --clean --no-install`. Install CocoaPods, run `pnpm run pods` (the wrapper sets `LANG` and `LC_ALL` to `en_US.UTF-8`), and open the generated `ios/smalltalk.xcworkspace` in Xcode. For daily development, build the Debug scheme for an iOS Simulator and run `pnpm start` for Metro. Keep normal simulator code signing enabled: building with `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain, so device pairing fails. A physical device is optional for this development proof.

`app.config.js` selects the installed identity with `APP_VARIANT`:

| Variant | Installed identity | Bundle identifier | Use |
| --- | --- | --- | --- |
| `dev` (default) | Fractal Dev, amber DEV-badged icon | `com.compoundingtech.smalltalk.starter` | Debug with Metro, simulator and device smoke tests |
| `daily` | Fractal, ordinary icon | `com.compoundingtech.smalltalk` | Release build used every day on a phone |

The two apps coexist on one phone and keep separate Keychain profiles, so each is paired on its own. Any other value fails the configuration. Changing variants requires a clean prebuild, and the same `APP_VARIANT` must stay exported for the Xcode build that embeds the bundle. The EAS simulator profile selects dev; preview selects daily.

The DEV icon generator requires macOS (CoreGraphics, ImageIO and CoreText). Run `swift scripts/generate-dev-icon.swift assets/icon.png assets/icon-dev.png` from this directory. It makes its output read-only (0444); when regenerating, the script itself restores write permission (0644) before overwriting it, so no manual `chmod` is needed. To edit the generated PNG with another tool, first run `chmod u+w assets/icon-dev.png`.

For an offline device build of the daily app, export `APP_VARIANT=daily` for every step, run `pnpm exec expo prebuild --platform ios --clean --no-install`, install pods, run `pnpm run export:ios`, and build Release with local Apple Development signing and provisioning for that device. Keep `APP_VARIANT=daily` exported for the Xcode build that embeds the bundle. The Release bundle is embedded and runs without Metro. No Expo account, EAS service, App Store, or Shareup signing is part of this path.

Before exporting or building the daily app, run this guard from this directory in the same shell as the build. It rejects a missing daily environment and a stale dev native project rather than silently building the wrong identity:

```sh
test "${APP_VARIANT-}" = daily &&
  xcodebuild -project ios/smalltalk.xcodeproj -target smalltalk -configuration Release -showBuildSettings |
  grep -Eq '^[[:space:]]*PRODUCT_BUNDLE_IDENTIFIER = com[.]compoundingtech[.]smalltalk[[:space:]]*$' ||
  { echo "Daily identity mismatch: export APP_VARIANT=daily, clean prebuild, and reinstall pods before building." >&2; exit 1; }
```

The private-range and `.local` HTTP ATS exceptions intentionally apply to the daily Release app too: its paired gateway can be reached over encrypted Tailscale or a trusted LAN, not only public HTTPS. This does not make LAN HTTP encrypted; the credential/data exposure warning and gateway URL restrictions described above still apply. Prefer HTTPS on an untrusted LAN.

Daily uses the `com.compoundingtech.smalltalk` URL scheme, separate from the dev starter scheme. It does not accept the starter-scheme pairing or `fabric-proof` links; those remain Debug-only development/proof tools, not a daily provisioning path. Pair the daily app through its form as described under Connect.

### Automatic daily Release delivery

`scripts/release_delivery.py` reconciles a tracked Git ref to the daily device
app (`com.compoundingtech.smalltalk`), without Metro, EAS, or OTA. The tracked
ref must support `APP_VARIANT=daily`; a ref without that variant is refused
rather than replacing the separate Debug app. First prepare a signed artifact
without phone access:

```sh
python3 apps/ios/scripts/release_delivery.py \
  --repo "$SOURCE_REPO" --state "$DELIVERY_STATE" \
  --expected-origin "$TRUSTED_ORIGIN_URL" --trusted-commit "$REVIEWED_FULL_SHA" \
  --device "$ST_IOS_DEVICE" --team "$ST_IOS_TEAM" \
  --developer-dir "$DEVELOPER_DIR" --build-only
```

The script starts nothing by itself: delivery runs only when a separately
configured host scheduler invokes it, and this repository ships no scheduler.

The default ref is protected `origin/main`. The device owner must approve
the tracked branch and its protection before scheduling delivery; repository
maintainers own branch protection. Supply the exact approved origin URL with
`--expected-origin` / `ST_IOS_EXPECTED_ORIGIN`, and an independently reviewed
full 40-character bootstrap commit with `--trusted-commit` /
`ST_IOS_TRUSTED_COMMIT`. These must come from trusted operator configuration,
not the fetched ref. The script refuses an origin mismatch before fetching,
and refuses any fetched commit not descended from its persisted `trusted_head`
(initially the bootstrap commit), before inspecting manifests or running code.
Accepted fetches atomically advance that head even if a later build fails.
The trust configuration is bound to the state directory; changing it requires
explicit reprovisioning, not silently reusing another trust boundary.

This deliberately refuses historical ref rewinds and divergent branch
switches. Rollback must be a new reviewed revert commit descended from the
last trusted head. There is no signature check; the origin pin and ancestry
boundary do not make a malicious maintainer-approved fast-forward safe.
Do not point the script at a contributor branch. `--ref` / `ST_IOS_REF`
accepts a valid branch (`origin/<branch>` or local) or a full commit SHA,
subject to that same ancestry boundary. Remote branches are fetched directly
from the source repository's approved `origin`; the source checkout is never
changed. A private dedicated clone, signed artifacts, and retry state live
under `--state`. Use a separate state directory for each device/team.

The tracked ref's code runs with the signing identity, login keychain, and
inherited environment: Expo configuration/plugins, Metro configuration,
podspec Ruby, `export:ios`, and Xcode build phases. The workspace install passes
`--ignore-scripts`, disabling dependency lifecycle scripts even if the ref
drops the workspace setting; it does not sandbox tracked `.pnpmfile.cjs`,
`.npmrc`, or explicitly invoked build scripts. Persistent `node_modules`,
native inputs, and DerivedData can be poisoned by an untrusted ref; clear
those caches before resuming trusted builds after such exposure.
Use `--build-only` or `ST_IOS_BUILD_ONLY=1` to prepare a signed artifact while
phone access is held. Installation and launch require the caller to supply
`AGENT_ACTION_APPROVAL=deploy`; without it, the signed artifact is retained
and no phone command runs. Remove `--build-only` and supply that approval
only after the device owner authorizes delivery; the retained artifact is
reused without rebuilding. Before signing, the generated app's `Info.plist` receives the full
Git revision as `StBuildCommit` and the revision's commit count as
`CFBundleVersion`. The artifact's `success.json` records both. Compare its
`build_version` with `devicectl device info apps --bundle-id
com.compoundingtech.smalltalk --device "$ST_IOS_DEVICE" --json-output -` to
verify the installed build independently of pairing or proof-only UI.
The embedded `EXPO_PUBLIC_ST3_BUILD` revision also appears in connected-client
diagnostics.

Prerequisites are Python 3, Git, Node 24, pnpm 12.7.0, CocoaPods, Xcode, and `gate-slot`
in the scheduler's PATH. The build takes a heavy admission slot, runs under
`taskpolicy -c utility` and `nice -n 19`, and caps Xcode parallelism at four jobs.
Utility QoS yields to interactive work without restricting a native build to
efficiency cores as background QoS does. The scheduler itself must not inherit
background QoS or low-priority I/O: `taskpolicy -c utility` is a clamp, not an
elevation from background. Missing admission tooling refuses the build; it
never silently bypasses host limits.
An advisory lock makes scheduled and manual invocations single-flight.
One root `pnpm install --frozen-lockfile` installs the workspace. Delivery follows
the app's recursive `workspace:` and `file:` runtime dependencies (currently
`clients/typescript/st3-views` and `clients/typescript/st3-client`). Changes under
that closure or `apps/ios`, or to root `package.json`, `pnpm-lock.yaml`,
`pnpm-workspace.yaml`, `.npmrc`, or `.pnpmfile.cjs`, cause a build; server-only changes do not.

The dedicated checkout retains pnpm installations and generated native inputs.
`build-cache/` stores their input identities and shared Xcode DerivedData,
including partial compilation from unsuccessful attempts. The frozen workspace
install reruns only when a package manifest, root lock/workspace configuration,
or Node/pnpm path changes, or `node_modules` is missing. Native generation and CocoaPods rerun only when
the package locks/manifests, evaluated app configuration, config plugins,
native modules, assets, or immutable toolchain paths change, or their generated
workspace/lock is missing. JS-only changes reuse the native cache.
The worker uses `pnpm exec expo config` and clean `pnpm exec expo prebuild`,
then `pnpm run export:ios`, all with `APP_VARIANT=daily` and the daily fabric
settings pinned before invoking Xcode.
Metro inlines `EXPO_PUBLIC_*` values into each transformed module, so the build runs
with a delivery-scoped `TMPDIR` under `build-cache/tmp` and resets that
transform cache whenever the inlined values change; a successful build also
fails if the bundle does not embed the current revision. Each
successful signed app is copied to its own immutable commit artifact before
it becomes eligible for installation, so another build cannot mutate a
pending install. Clear `build-cache/` after an in-place upgrade of a mutable
Node/CocoaPods/Xcode installation; versioned or Nix tool paths invalidate it
automatically. Do not delete a pending commit artifact.

Release signing uses `CODE_SIGN_STYLE=Automatic`, the supplied development
team, `Apple Development`, and `-allowProvisioningUpdates`. Set up the Xcode
account, development certificate/private key, unlocked keychain, and device
provisioning once interactively. Then prove a complete build from the same
unattended scheduler environment: a successful interactive Xcode build alone
does not prove its keychain/account access. Device/team IDs and signing
material belong in private environment/configuration, never in this repo.

Delivery compares content, not commits. The observed head is the tracked ref
from the last successful fetch, updated on every fetch, including reverts and
app-irrelevant commits. Its content key hashes the Git trees of `apps/ios` and
its local-package closure together with the inputs outside the revision: the
Xcode directory, Node/pnpm/CocoaPods paths, inherited `EXPO_PUBLIC_*` values,
and the pinned native environment. Builds always run with the daily Release
values `APP_VARIANT=daily`, `ST3_FABRIC_PROOF=0` (read by `StFabric.podspec`),
and `ST3_FABRIC_OFFLINE_DEBUG=0` (read by `metro.config.js`), so an inherited
value never reaches prebuild, CocoaPods, Metro, or Xcode; the pins are also part
of the native-generation cache key. A signed artifact is installed only if its
key equals the observed head's key under the current inputs. A build runs only
when no retained artifact or installed app has that key; a revert to retained
content reuses its artifact if that does not lower the installed version.
`CFBundleVersion` comes from a persisted counter, the larger of the commit
count and the last assigned version plus one, so it never decreases across
reviewed revert commits and forward branch switches. Every install checks it against the
installed version; an artifact with a lower version is rebuilt instead.
A pending artifact for other content is dropped, never installed.
An unsuccessful build cannot be installed. A failed content key is retried
after 15 minutes, then after 30, doubling up to six hours (`--build-backoff`
or `ST_IOS_BUILD_BACKOFF` sets the first delay in seconds); new content, such
as a fix, revert, or input change, builds on the next tick. A successful build is retained
until installation and launch both succeed; an unreachable or locked phone
is retried on the next tick without rebuilding. While the ref cannot be
fetched, the last observed head is used, never an older revision, and is
rebuilt from the local checkout if needed (deferred if it is missing there).
An origin mismatch is not an offline condition and never uses a cached target.
Offline delivery requires an existing observed commit equal to the persisted
trusted head; a fresh state cannot bless a cached ref when fetch fails.
The script never grants itself deployment approval. An operator supplies
`AGENT_ACTION_APPROVAL=deploy` for an approved invocation, or the separately
approved host scheduler sets it deliberately in its job environment. Enabling
that scheduler is standing deployment authorization for its configured device
and tracked branch and requires the device owner's explicit approval; it
remains held separately from this script PR and any outage hold must be lifted
by its owner. Run an approved scheduler every five minutes while the Mac is
awake and on the phone's LAN. Neither pairing nor the Debug bundle is touched.
Xcode 27's process listing exposes executable paths and PIDs, not foreground
state: the scheduler conservatively treats a running daily app as active and
defers installation until 40 minutes after observing the change. After that,
delivery takes priority even if the app is running. With five-minute polling,
this leaves time within the one-hour delivery target for installation; build,
admission, provisioning, and device failures remain visible rather than being
reported as delivered. No launch uses `--terminate-existing`.
Use `--max-deferral` or `ST_IOS_MAX_DEFERRAL` to configure that courtesy window;
zero is appropriate for a coordinated proof when the phone is not being used.

Every invocation records outcomes to `outcomes.jsonl` and stdout; each build
has `artifacts/<commit>/build.log`. `state.json` records the observed head and
its content key, the pending signed artifact, the installed content, and the
installed and last assigned build versions. Every state file is written to a
temporary file, fsynced, and renamed over the old one. An unreadable
`state.json` is never replaced with fresh state, which could lower
`CFBundleVersion`: the tick records `state_corrupt`, exits nonzero, and neither
builds nor touches the phone until an operator restores or repairs the file.
Legacy state without a trust anchor is also refused; do not delete it to
silently reset the authoritative version floor. Reprovisioning is an operator
action that must preserve the installed/assigned build-version floor and set
the independently reviewed trust configuration.

Before installation the script persists an independent `install_attempt`
artifact record, retained even when a build-only tick supersedes its pending
artifact. If interrupted between device installation and saving success,
the next approved tick queries `devicectl device info apps` into a private JSON file
and removes the receipt after parsing. An exact daily bundle ID and numeric
`bundleVersion` (not the marketing version) establish whether installation
already completed, so launch can resume without a second install. A higher
device version raises the version floor and refuses the older artifact.
Query failure, malformed JSON, duplicate daily entries, or invalid versions
defer delivery without assuming success or mutating the phone. An absent
daily app permits retry; any previously persisted version floor is retained.
A readback contradicting the cached installed version also invalidates the
cached installed-content identity, while retaining the monotonic version floor.
No device query runs under `--build-only` or without caller deploy approval.
Optionally set both `--notify-from` and `--notify-to` (or
`ST_IOS_NOTIFY_FROM`/`ST_IOS_NOTIFY_TO`) to send state changes with
`st conversations send`: each newly installed commit, a new build failure,
or device unavailability lasting over one hour (once until recovery).
A notification carries only the outcome, time, commit, build version, and
exit status; raw tool errors and log paths, which can name the device or
signing identity, stay in `outcomes.jsonl`.
Unchanged ticks stay in the log only. Notification failures do not discard
delivery state or stop installation.
Keep all logs and state private: native tools include signing/device details.

Focused scheduler tests: `python3 -m unittest discover -s apps/ios/scripts -p 'test_release_delivery.py'`.

The Debug app accepts a short-lived pairing deep link for headless simulator checks: `com.compoundingtech.smalltalk.starter://pair?gateway=...&id=...&code=...`. The link opens the pairing form with the code prefilled; it does not submit the code until a separately supplied fingerprint or explicit unpinned override is chosen. The handler is disabled in Release. Treat the link as a temporary credential and do not commit or log its populated form.

For connected Debug smoke tests, `com.compoundingtech.smalltalk.starter://tab/Fleet` opens a tab (only Now, Resources, Fleet and Spaces are accepted), `com.compoundingtech.smalltalk.starter://mission?id=mission/...` opens a mission, `com.compoundingtech.smalltalk.starter://agent?id=agent/...` opens an agent's conversation, and `com.compoundingtech.smalltalk.starter://session?id=session/...` opens an exact conversation. Add `&terminal=terminal/...` to the session link to inspect its live terminal screen. `com.compoundingtech.smalltalk.starter://scroll?y=800` scrolls the visible list for screenshots. These links are disabled in Release and carry no authorization: the already-paired client still has to pass the gateway's normal checks.
`com.compoundingtech.smalltalk.starter://attention?id=attention/...` opens one attention item's detail from Now.

The simulator asks before it opens each link it is handed, so a headless run can instead put links in `apps/ios/.env` for Metro to inline into the Debug bundle: `EXPO_PUBLIC_ST3_TEST_PAIR_LINK` opens the pairing form at launch, `EXPO_PUBLIC_ST3_TEST_TAB` picks the first tab, and `EXPO_PUBLIC_ST3_TEST_LINKS` (space-separated) follows each link six seconds apart. Restart Metro with `--clear` after changing them.

For screenshots without a real st, `node demoGateway.mjs 8791` serves invented data: attention, missions, agents, one conversation, and one terminal. Pair a Debug simulator with `com.compoundingtech.smalltalk.starter://pair?gateway=http://<the Mac's LAN or Tailscale IPv4>:8791&id=demo&code=demo`. It is not authenticated; never point a Release build or a device at it.
It completes the phone's real device-key verification read-only: the unauthenticated capabilities preflight is the raw advertisement, and pairing answers with an invented host-signed person-root proof and no signing grant, so the phone still runs its own checks.

Generated `ios/`, build output, signing material, local configuration, and proof screenshots are ignored by Git. Do not commit Apple team/device IDs, credentials, machine paths, or private network addresses. Expo SDK 57 needs the `expo-build-properties` scene-lifecycle opt-in for Xcode 27/iOS 27.

## Fabric, an opt-in carrier

The app reaches its member over its saved gateway (Tailscale, the local network or HTTPS) unless the person turns on **Fabric** under Fleet › carrier. Fabric needs a build that links the native bridge: build with `ST3_FABRIC=1 pnpm run pods` after building the Rust framework (see [the module's README](modules/st-fabric/README.md)). A build without it is the ordinary app and shows no carrier setting.

With Fabric on, each foreground opens a fresh native listener on the loopback address the bridge returns (the only loopback address the app accepts; it is never saved), the client talks to it, and the background stops it. The paired bearer and the enrolled signing key are the same ones the saved gateway uses, so a device already paired to the member needs only the setting and a grant, not a new pairing. A new pairing made while the bridge is open goes over fabric. The member exposes its paired-only client socket as one fabric service and grants this phone's node ID (shown in Fleet › carrier) that one service, inbound-only: `fabric add PHONE_NODE_ID NAME --inbound-only --allow SERVICE` on fabric 0.2.41 or later, so the member never dials or probes the phone ([grant](../../docs/st3/ios-fabric-protocol.md#member-exposure-and-authorization)).

The target is `node=NODE_ID&service=NAME` (and an optional `addr=` hint): paste it under Fleet › carrier. A build can carry one: put `EXPO_PUBLIC_ST3_FABRIC_DEFAULT='node=…&service=…'` in an ignored `.env` on the build host and that build opens on Fabric with the target preset, until the person chooses otherwise. A build from the repository carries none and stays on its saved gateway. Never commit a target, a node ID or a link.

If fabric cannot connect (no grant, member out of reach, no answer within four seconds) the app falls back to the saved gateway and names the route and the reason in Fleet › this connection; a member that refuses the grant is named as a refusal. Turn the fallback off under Fleet › carrier to see fabric fail instead. Fabric is not tried again for thirty seconds after it fails, and a lost connection is redialed three times before the app falls back. Nothing is queued while the bridge is down, and a mutation is never retried over a new connection.

The Debug-only fabric proof (`fabric-proof` links, the separate offline Debug app and its isolated member helper) is unchanged; see [the module](modules/st-fabric/README.md).

## Resource sidebar

Resources opens shared folders followed by Everything else, grouped by Agents, Missions,
Terminals and Machines. Agents and Missions open their full searchable lists in one tap. The filter row stays above the person's folders. This view follows
the optional arrangements stream; older pairings retain the grouped resource browser.
Tap a resource to open it and a heading to collapse it. Long-press a resource for Move to;
long-press a folder for rename or confirmed deletion. Each folder's + finds a resource to
add; choosing it previews the move, and the Move button commits it. Drag handles reorder folders and move or reorder resources; taps never move rows.
Folder groups start open; busy groups start collapsed above 30 items. Device-local choices
and previously seen rows are scoped to the gateway and person. Missing rows stay marked
unavailable. Edits use `arrangement.edit` and the live winning registers, with Sidebar
created only on the first edit.

The tabs are Now, Resources, Fleet and Spaces. Earlier tab aliases, tab links and stored
tab orders are intentionally retired. A fresh workspace names its host and points to
Now → New → New terminal; New mission is available there too.

Pull requests, repositories and other observed graph resources are available through the
existing optional `resources.list` route. Resources refreshes three bounded 200-item windows
when it opens or is pulled to refresh; there is no catalog polling or additional collection
subscription. Truncation or an unsupported read is visible while the ordinary browser stays
usable. Resource details show the observed kind and facts with an explicit Open link button,
and resource tabs open from Spaces. The folder's + finder can also file an explicitly typed
graph subject. Session and PTY IDs only navigate; placements require stable graph subjects.

Opening a resource detail also reads its exact subject with the existing prefix filter,
including subjects beyond the catalog windows.

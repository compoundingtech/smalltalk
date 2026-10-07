# Build and run the iOS app

The Smalltalk iOS app is a client for your fleet: it shows Home, Agents, Missions, and Fleet, and lets you talk to seats. It connects through a paired gateway; the phone does not run an agent or replicate the graph.

Use a Mac with **full Xcode**, its command-line tools selected, and an installed iOS Simulator runtime. This walkthrough uses Xcode 27; [Expo's simulator setup](https://docs.expo.dev/workflow/ios-simulator/) shows where to select the tools and download a runtime. Open Xcode once and finish its first-run setup. The native Xcode, CocoaPods, simulator, and device steps are **not yet verified on a fresh Mac**.

You also need Git, Node.js 24 with npm, and CocoaPods. Install them with your usual package manager. If you use Nix, enter this shell **on the Mac** before running this page's commands:

```sh
nix --extra-experimental-features 'nix-command flakes' shell \
  nixpkgs#nodejs_24 nixpkgs#cocoapods --command bash
```

Nix supplies these tools; Xcode and the simulator runtime still come from Apple. Check the selected tools:

```sh
node --version
npm --version
pod --version
xcodebuild -version
```

## Prepare the app

Clone into a new directory, or use your existing checkout:

```sh
mkdir -p ~/src
git clone --depth 1 https://github.com/compoundingtech/smalltalk.git ~/src/smalltalk
cd ~/src/smalltalk/apps/ios
npm ci
npm run typecheck
npm test
APP_VARIANT=dev npx expo prebuild --platform ios --clean --no-install
npm run pods
open ios/*.xcworkspace
```

`prebuild --clean` regenerates the ignored `ios/` directory from `apps/ios/app.config.js` and its shared `app.json` settings. `APP_VARIANT=dev` (the default) installs **Smalltalk Dev**, bundle `com.compoundingtech.smalltalk.starter`, with a DEV-badged icon and OTA disabled. `APP_VARIANT=daily` installs **Smalltalk**, bundle `com.compoundingtech.smalltalk`, with signed paired-gateway OTA. They coexist and have separate Keychain profiles. Keep native changes in checked-in Expo modules/config plugins before regenerating; switching variants requires a clean prebuild and new pods. `npm run pods` supplies the UTF-8 locale CocoaPods needs. Open the **workspace**, which includes the pods, rather than the Xcode project.

For daily builds, first generate your own update signing identity and set `ST_IOS_UPDATES_CERT`
to its public certificate path as described in the [publication guide](../apps/ios/README.md#local-signing-and-publication).
Daily fails closed without that setting; no deployer's certificate/key is bundled with this public repo.

## Run in the simulator

In Xcode, select the generated app scheme and an installed iPhone simulator. Use the scheme's **Debug** configuration and press **Run**. Keep normal simulator code signing enabled: `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain and pairing fails.

In another terminal, start Metro from the app directory:

```sh
cd ~/src/smalltalk/apps/ios
npm run start
```

Keep Metro running while using the Debug app. If it opens before Metro is ready, reload it after the server starts. On Xcode 27, simulated devices appear in **Device Hub**; Xcode 26 uses **Simulator**. This is a local native build with the app's own modules. No Expo account, EAS service, or App Store upload is needed.

## Pair with your daemon

Finish [getting started](getting-started.md) on the daemon machine. Make its **paired-only gateway** reachable from the simulator or phone using [gateway setup](st3/client-v0/README.md#tailnet-carrier). The gateway is a separate listener from the privileged daemon socket; never expose `st3.sock`.

On the daemon machine, begin pairing with your own person identity:

```sh
st devices --as person/ada pair --full-control 'Garden iPhone'
```

In the app, enter the gateway URL, pairing ID, and code. Use HTTPS, or a gateway bound to the daemon's Tailscale IPv4 address with `http://100.x.y.z:port`; Tailscale encrypts that route. Private LAN and `.local` HTTP routes also work, but carry credentials and data without transport encryption. The app explains this when you choose one. The gateway needs to accept both HTTP requests and WebSockets.

Open **Agents**, select `garden/worker`, and send a message. Home shows what needs you. If pairing succeeds but controls are unavailable, check that you paired with `--full-control`; re-pair a previously limited device. A dropped connection shows offline/reconnect state; reconnect before sending an action. See [the phone connection guide](../apps/ios/README.md#connect) for scopes and client details.

## Put it on your iPhone

For a Debug build, connect the phone to the Mac, enable its Developer Mode when asked, and choose it as Xcode's run destination. In **Signing & Capabilities**, select your own Apple Development team and let Xcode manage provisioning. If that team cannot register the default bundle identifier, choose your own unique identifier for this local target; regenerating `ios/` resets that local change. Run the app; keep Metro reachable from the phone. Do not commit team IDs, provisioning profiles, device IDs, or pairing credentials.

For the daily app that runs without Metro, regenerate the native project for the daily variant and export the iOS JavaScript:

```sh
cd ~/src/smalltalk/apps/ios
APP_VARIANT=daily npx expo prebuild --platform ios --clean --no-install
npm run pods
APP_VARIANT=daily npm run export:ios
open ios/*.xcworkspace
```

Set the generated scheme's **Run → Build Configuration** to **Release**, choose your phone, and build/run with its Apple Development signing and provisioning. Release embeds the bundle and can run with Metro stopped. It still needs the gateway connection to read current work and send actions. Keep the daily bundle identifier unchanged: update authentication and the native gateway bridge target `com.compoundingtech.smalltalk`. The config plugin pins the variant in generated `ios/.xcode.env` for Xcode's bundle/resource phases; do not override it in `.xcode.env.local`.

The daily build starts offline from its embedded/cached verified bundle and checks only after the paired credential has been read from Keychain. Only a narrow 15-minute app/channel-bound token reaches Expo's persisted header override. Downloads require explicit foreground restart consent or wait for the next app launch; native recovery/anti-bricking remains enabled. See [daily signing, publication and on-phone acceptance](../apps/ios/README.md#daily-signed-app-updates) for local publisher commands, key custody, compatibility/runtime changes, and the release-device test plan. Never put the private manifest signing key in st3 or a gateway.

Keep pairing codes out of logs and Git. Debug-only pairing deep links and an invented-data demo gateway are documented in [the app README](../apps/ios/README.md#build-locally) for development checks. See [Expo local development](https://docs.expo.dev/guides/local-app-development/) for native rebuilds and Metro troubleshooting.

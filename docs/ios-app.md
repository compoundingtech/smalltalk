# Build and run the iOS app

The Smalltalk iOS app is a client for your fleet: it shows Home, Agents, Missions, and Fleet, and lets you talk to seats. It connects through a paired gateway; the phone does not run an agent or replicate the graph.

Use a Mac with **full Xcode**, its command-line tools selected, and an installed iOS Simulator runtime. This walkthrough uses Xcode 27; [Expo's simulator setup](https://docs.expo.dev/workflow/ios-simulator/) shows where to select the tools and download a runtime. Open Xcode once and finish its first-run setup. The native Xcode, CocoaPods, simulator, and device steps are **not yet verified on a fresh Mac**.

You also need Git, Node.js 24 with Corepack, and CocoaPods. Install them with your usual package manager. Outside Nix, run `corepack enable`; the root manifest provisions pnpm 12.7.0. If you use Nix, enter the web shell from the repository root **on the Mac**:

```sh
nix develop .#web
```

The web shell supplies pnpm 12.7.0, Node 24.20, Bun 1.4.2 and Buck2. CocoaPods must also be installed; Xcode and the simulator runtime come from Apple. Check the selected tools from the repository root:

```sh
node --version
pnpm --version
pod --version
xcodebuild -version
```

## Prepare the app

Clone into a new directory, or use your existing checkout:

```sh
mkdir -p ~/src
git clone --depth 1 https://github.com/compoundingtech/smalltalk.git ~/src/smalltalk
cd ~/src/smalltalk
corepack enable # Outside the Nix web shell
pnpm install --frozen-lockfile
cd apps/ios
pnpm typecheck
pnpm test
pnpm exec expo prebuild --platform ios --clean --no-install
pnpm run pods
open ios/smalltalk.xcworkspace
```

The root frozen install covers the client, shared views and iOS packages. `prebuild --clean` regenerates the ignored `ios/` directory from app configuration. Keep native changes in the checked-in Expo modules or configuration before regenerating. `pnpm run pods` supplies the UTF-8 locale CocoaPods needs. Open the **workspace**, which includes the pods, rather than the Xcode project.

## Run in the simulator

In Xcode, select the **smalltalk** scheme and an installed iPhone simulator. Use the scheme's **Debug** configuration and press **Run**. Keep normal simulator code signing enabled: `CODE_SIGNING_ALLOWED=NO` leaves the app without a usable Keychain and pairing fails.

In another terminal, start Metro from the app directory:

```sh
cd ~/src/smalltalk/apps/ios
pnpm start
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

For an app that runs without Metro, export the iOS JavaScript first:

```sh
cd ~/src/smalltalk/apps/ios
pnpm run export:ios
```

Set the scheme's **Run → Build Configuration** to **Release**, choose your phone, and build/run with its Apple Development signing and provisioning. Release embeds the bundle and can run with Metro stopped. It still needs the gateway connection to read current work and send actions.

Keep pairing codes out of logs and Git. Debug-only pairing deep links and an invented-data demo gateway are documented in [the app README](../apps/ios/README.md#build-locally) for development checks. See [Expo local development](https://docs.expo.dev/guides/local-app-development/) for native rebuilds and Metro troubleshooting.

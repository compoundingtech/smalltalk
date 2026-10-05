# Using stui and the iOS app

Both clients show the work in your fleet. Finish [getting started](getting-started.md) on the daemon machine first.

## Use stui

In a terminal on that machine, run:

```sh
stui
```

stui opens a space with tabs and splits. **Ctrl+S** shows or hides the sidebar; choose **Agents** or **Missions** there. **Ctrl+K** finds an agent, mission, machine, space, or text in conversations; type a name and press **Enter** to open it.

- **Home** shows attention items: decisions, requests, unread messages, and failures that need you. Open it with **Ctrl+H** when you are not typing, or click the need-you count. Follow each card's displayed actions; opening a card does not approve it.
- **Agents** opens a seat's conversation, with messages, the harness transcript, and tool output. Scroll to read earlier entries; type in the message box and press **Enter** to reply.
- **Missions** shows each run's steps, progress, waiting reason, and who is working on it.
- **Usage** shows observed tokens, estimated cost, and account limits. Click **$ usage** in the top bar (widen the terminal if needed); **b** changes grouping and **p** changes the period.

In an agent's conversation, **Ctrl+]** attaches its live terminal. Finish harness login or trust prompts there; **Ctrl+\\** detaches back to the conversation. Detach before using stui's navigation keys. **Ctrl+Q** quits stui; the daemon, agents, and their work keep running. See the [terminal client reference](../crates/stui/README.md) for more controls.

### Connect from another machine

Install stui on the client machine. Publish the daemon's **paired-only gateway** with the [gateway setup](st3/client-v0/README.md#tailnet-carrier), using HTTPS or an encrypted tailnet route. On the daemon machine, make a pairing challenge:

```sh
st devices --as person/ada pair --full-control 'Garden laptop'
```

On the client machine, use your gateway URL and the returned pairing ID:

```sh
printf 'Gateway URL: '; read -r st_gateway_url
printf 'Pairing ID: '; read -r st_pairing_id
stui pair "$st_gateway_url" "$st_pairing_id"
stui --client
```

Enter the single-use code privately when prompted. The saved pairing supplies your person identity and lets stui connect without a local daemon. See [client-only setup](st3/client-only.md) for multiple members and reconnecting.

## Put the iOS app on your phone

There is no App Store or TestFlight build; build it yourself on a Mac with full Xcode, Node.js 24, and CocoaPods. From a new checkout:

```sh
mkdir -p ~/src
git clone https://github.com/compoundingtech/smalltalk.git ~/src/smalltalk
cd ~/src/smalltalk
npm ci --prefix clients/typescript/st3-views --ignore-scripts --no-audit --no-fund
cd apps/ios
npm ci
npx expo prebuild --platform ios --clean --no-install
npm run pods
npm run export:ios
open ios/smalltalk.xcworkspace
```

In Xcode, select the **smalltalk** scheme and your connected iPhone. Under **Signing & Capabilities**, choose your own Apple Development team and automatic provisioning; use a unique bundle identifier if your team cannot register the default. Enable Developer Mode on the phone when asked. From `apps/ios`, [build and install](https://docs.expo.dev/guides/local-app-development/) onto the phone:

```sh
npx expo run:ios --device --configuration Release
```

Choose your iPhone when prompted. Release embeds the JavaScript so you can use the app without Metro.

On your daemon machine, make a separate challenge for the phone:

```sh
st devices --as person/ada pair --full-control 'Garden iPhone'
```

In the app, enter the reachable gateway URL, pairing ID, and code. Open **Agents**, select your worker, and send a message. [Build and run the iOS app](ios-app.md) has the Xcode prerequisites, simulator and Debug workflow, signing details, and pairing troubleshooting.

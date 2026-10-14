# Using the terminal UI and the iOS app

Both clients show the work in your fleet. Finish [getting started](getting-started.md) on the daemon machine first.

## Use the terminal UI

In a terminal on that machine, run:

```sh
st
```

The terminal UI opens a space with tabs and splits. **Ctrl+S** shows or hides the sidebar; choose **Agents** or **Missions** there. **Ctrl+K** finds an agent, mission, machine, space, or text in conversations; type a name and press **Enter** to open it.

- **Home** (Now) lists your alerts, then updates that ask nothing. An alert is anything waiting on your answer: an ask, a review gate, a launch or revision approval, a harness prompt or login. Open it with **Ctrl+H** when you are not typing, or click the alert count in the top bar; the bar prints nothing when there are none. Each alert also shows in its agent's conversation, above the message box, as one line; click it or press **Ctrl+N** to expand it in place to the same card Now shows; a click on its top line, or **Esc** while the card has the keys, folds it back.

A harness prompt's card reads like the harness's own dialog: what it asks about (the command, edit or plan in full), its question line ("Allow this read outside the working directories?"), and its options numbered in the harness's order and words ("1. Yes, but ask again next time"), with one marked **❯**. The card offers only the answers st can send, so the harness's other choices (keep allowing, auto-accept edits) stay in its terminal, and the card says so. A Claude question shows each question with its options to choose (several for a multi-select) before **Submit answers**; a screen prompt (a new MCP server, workspace trust) and a call Claude's auto mode denied show their own answers. Web, tui and ios share these words: `clients/typescript/st3-views/promptForm.ts` and `crates/st3-ui-model/src/prompts.rs`.

Answering a prompt's card:

- **Click** an option to answer it. A refusal (No, exit, quit) is a plain option and asks nothing more.
- **Ctrl+N** in a conversation, or **a** on the card in Now, gives the card the keys. **↑/↓** and **Tab** move the mark and answer nothing. **1–9** answers that option, or chooses it for a question. **Enter** answers the marked option only when it is a safe one about what is shown in full (allow a call shown whole, approve a plan shown whole, submit your own choices, continue without an MCP server), never a refusal. **Esc** gives the keys back.
- Typing never waits on a card: a letter goes straight to the message box with the keys.
- A call too long for the band is cut there and its allow withheld; the card in Now (Ctrl+H) shows it whole and offers it.

Follow each card's displayed actions; opening a card does not approve it.
- **Agents** opens a seat's conversation, with messages, the harness transcript, and tool output. Scroll to read earlier entries; type in the message box and press **Enter** to reply.
- **Missions** shows each run's steps, progress, waiting reason, and who is working on it.
- **Usage** shows observed tokens, estimated cost, and account limits. Click **$ usage** in the top bar (widen the terminal if needed); **b** changes grouping and **p** changes the period.

In an agent's conversation, **Ctrl+]** attaches its live terminal. Finish harness login or trust prompts there; **Ctrl+\\** detaches back to the conversation. Detach before using the terminal UI's navigation keys. **Ctrl+Q** quits the terminal UI; the daemon, agents, and their work keep running. See the [terminal client reference](../crates/stui/README.md) for more controls.

### Connect from another machine

Install Smalltalk on the client machine. Publish the daemon's **paired-only gateway** with the [gateway setup](st3/client-v0/README.md#tailnet-carrier), using HTTPS or an encrypted tailnet route. On the daemon machine, make a pairing challenge:

```sh
st devices --as person/ada pair --full-control 'Garden laptop'
```

On the client machine, use your gateway URL and the returned pairing ID:

```sh
printf 'Gateway URL: '; read -r st_gateway_url
printf 'Pairing ID: '; read -r st_pairing_id
# Set st_member_fingerprint to the fingerprint obtained through a trusted channel.
st devices complete "$st_gateway_url" "$st_pairing_id" --fingerprint "$st_member_fingerprint"
st ui --client
```

Enter the single-use code privately when prompted. The saved pairing supplies your person identity and lets the terminal UI connect without a local daemon. See [client-only setup](st3/client-only.md) for multiple members and reconnecting.

## Put the iOS app on your phone

There is no App Store or TestFlight build; build it yourself on a Mac with full Xcode, Node.js 24, and CocoaPods. From a new checkout:

```sh
mkdir -p ~/src
git clone https://github.com/compoundingtech/smalltalk.git ~/src/smalltalk
cd ~/src/smalltalk
corepack enable
pnpm install --frozen-lockfile
cd apps/ios
pnpm exec expo prebuild --platform ios --clean --no-install
pnpm run pods
pnpm run export:ios
open ios/smalltalk.xcworkspace
```

In Xcode, select the **smalltalk** scheme and your connected iPhone. Under **Signing & Capabilities**, choose your own Apple Development team and automatic provisioning; use a unique bundle identifier if your team cannot register the default. Enable Developer Mode on the phone when asked. From `apps/ios`, [build and install](https://docs.expo.dev/guides/local-app-development/) onto the phone:

```sh
pnpm exec expo run:ios --device --configuration Release
```

Choose your iPhone when prompted. Release embeds the JavaScript so you can use the app without Metro.

On your daemon machine, make a separate challenge for the phone:

```sh
st devices --as person/ada pair --full-control 'Garden iPhone'
```

In the app, enter the reachable gateway URL, pairing ID, and code. Open **Agents**, select your worker, and send a message. [Build and run the iOS app](ios-app.md) has the Xcode prerequisites, simulator and Debug workflow, signing details, and pairing troubleshooting.

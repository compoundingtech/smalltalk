# Fractal terminal client

`st` in a terminal opens spaces on the live graph: splits with their own tabs, each showing a conversation,
a mission, a terminal, Now or usage. `st ui --space NAME` opens a named space. `st ui --demo`
shows them on invented data and sends nothing. Spaces are stui's only layout: the layout from
before them is gone ([#1166](https://github.com/compoundingtech/smalltalk/issues/1166)), and
`st ui --classic` refuses with what to run instead.
The screens follow [docs/clients/ui-contract.md](../../docs/clients/ui-contract.md), shared with the
iOS app.

The TUI library connects through the generated Rust `st3.client.v0` client, the same typed data boundary
used by the CLI. Its subscriptions, reconnects, snapshot model and cache come from
[`st3-feed`](../st3-feed/README.md). It paints immediately, hydrates attention/agents/sessions first, then fills in
mission and fleet details without blocking keys. A private, actor-and-endpoint-scoped read-only
cache keeps the last snapshot visible while reconnecting. Actions still need a live connection
and fresh fences; there is no offline mutation queue.

Install the repo's `.#st3` Nix package and run `ST3_PERSON=person/<your-id> st` in a terminal
with a running st3 daemon. For source development, use
`ST3_PERSON=person/<your-id> cargo run -p st3 --locked -- ui`. `ST3_ENDPOINT` can override the discovered Unix socket. When `ST3_PERSON` is unset,
the TUI uses `person` from `~/.config/st3/config.toml` (or `$XDG_CONFIG_HOME/st3/config.toml`).
A concrete person identity is required so Now and devices show the right data.

For a laptop without a daemon, run `st devices complete MEMBER_URL PAIRING_ID --fingerprint SHA256` and enter the member's
single-use code, then run `st`. The paired member supplies the person identity. See
[client-only setup](../../docs/st3/client-only.md) for gateway setup, multiple members, private
credentials, offline cache, and automatic reconnect. `--client` requires a saved pairing;
`--local` selects the local daemon even when a pairing exists. While offline, the terminal UI keeps its last
display, shows the last connection time, queues no mutations, and offers `r` to retry now.

Keys (`?` shows them all): `Ctrl+K` opens anything (agents, missions, machines, spaces, what
was said in conversations, Now, Usage and the Fleet list); `Tab` and `Shift+Tab` move between a split's tabs; `Alt+arrows` move
between splits; `Ctrl+T`, `Ctrl+V`, `Ctrl+X` and `Ctrl+W` open a tab, split right or below, and
close a tab; `Ctrl+S` shows the sidebar and `Ctrl+H` opens Now over the space; `Ctrl+Q` quits.
Middle-click a tab to close it: press and release over the same tab. This uses the same
close action as `Ctrl+W` and the tab's right-click menu.
In a conversation, letters type into its message box and commands are chords: `Ctrl+F` finds,
`Ctrl+E` expands tool output, `Ctrl+P` simplifies, `Ctrl+D` shows the agent's details, `Ctrl+]`
attaches the agent's terminal and `Ctrl+\` leaves it. A drag selects text in one pane and copies
it on release.

Opening an agent attaches its terminal once its tab stays in front for a moment; `Ctrl+\` shows its
conversation. An agent without a terminal (a session st did not start, a harness without one) opens
its conversation. **Conversation first**, in `Ctrl+K`, makes this device open agents on their
conversation instead; it is kept in `$XDG_STATE_HOME/st3/stui/prefs.json` and never synced. A
device that saved the older `terminal_first` keeps attaching. An alert for the agent shows on top
of its attached terminal: click it to show it, click an answer to send it, or click
"open in Now". Keys always go to the terminal (`Ctrl+K` and `Ctrl+\` aside), so typing never
answers an alert. When the link to the
terminal drops, its status line says `reconnecting`, why, the next try and `Ctrl+\ conversation`;
it attaches again by itself, waiting 1, 2, 4… up to 32 seconds between tries. Scrolling to the top of a conversation loads the page before it. Now cards show
their own keys; `y` confirms what a card asks, and Enter never does. Now has no sections behind
it: no key, click or update turns it into another list, and Esc gives the keys back to the
focused split.

A seat can ask the interface to show something with `st ui open agent/NAME` (also `mission/NAME`,
`mission-run/NAME/RUN` and `machine/NAME`), `--split` and `--below` for a split, `--keep-focus` to leave
the person where they are. The command writes a small file into the configured person's private state
directory (`st3/stui/requests-KEY`) and waits for a terminal interface of that person on the same machine
to answer: opened, not found, or nobody answered, when nothing is left queued. A pane never takes the
focus from a message being typed, and a subject st has not listed yet is waited for a few seconds, so an
agent made a moment ago can be opened. A paired device does not read the directory. The
mission view stays on its mission from step to step; between steps its card says what is up next.

The [terminal tab input reference](../../docs/stui-terminal-tab.md) records the measured
input modes, query replies, and remaining gaps, including the headless PTY probe.

Verification:

```sh
cargo test -p stui --locked --lib
cargo test -p st3 --locked --test integration typed_keys
cargo test -p st3 --locked --test integration client_only
cargo build -p st3 --locked
ST3_PERSON=person/<your-id> python3 crates/stui/tests/pty_smoke.py target/debug/st3 --ui
```

The PTY smoke test runs with or without a local daemon or st config. It checks first-frame and key-to-redraw
latency, also against a deliberately stalled getter, plus alternate-screen restoration after
normal exit, SIGTERM, a long-running PTY hangup, and a debug panic. For a packaged release
binary, pass its path followed by `--ui --no-panic`; the release build has no debug panic hook. The two
`attach_default_probe` (run by `cargo test -p stui --lib`, beside `terminal_tab_protocols`) drives a
real terminal tab in PTYs through the default attach, conversation first, an alert over the
terminal and a dropped link. The two
QA scripts need a live daemon and the `pty` executable: one opens an agent from `Ctrl+K`, scrolls
it and drags to copy; the other attaches the agent's terminal and leaves it by `Ctrl+\` and by a
click on its header. Use a person of their own (such as `person/<qa-name>`) so they mark nothing
read for anyone.

## One cold conversation-open timing

For a supported diagnostic in a real terminal, set `STUI_OPEN_TRACE_FILE` to a new private
file and `STUI_OPEN_TRACE_CONVERSATION` to the exact agent or session being opened. Leave
both unset for ordinary use. The selector only observes the UI: it does not open a tab,
change the actor/endpoint, attach a terminal or alter subscriptions. An existing file is
never appended to or overwritten; a new file is mode 0600 on Unix.

Start the installed `st` once, using the person's ordinary pairing/configuration and
space. Either restore a space with that conversation in front, or open it normally with
Ctrl+K and Enter. Preserve caches, preferences and daemon state. Do not use Ctrl+] (terminal
attachment), `--demo`, or a headless test client as a conversation-render measurement.

The file has at most seven content-free JSON lines (at most 2 KiB), with monotonic `at_ms`
from entry to `main`: `process_started`, `conversation_intent`, `converse_enqueued`,
`conversation_update`, `nonempty_update`, then `first_useful_presented`. A useful cached
presentation may precede the live update and end the trace earlier; each record explicitly
states `live_update_seen` and `live_nonempty_update_seen`. Intent means the
UI loop observed the selected conversation among its wanted subscriptions, not the exact
key-press time. Enqueued means the feed command channel accepted it, not that a WebSocket
send completed. The first update may be empty. The last marker requires an unobscured
conversation entry and follows the successful terminal draw/flush and synchronized-update
end; it excludes loading, starting, empty, header-only and off-screen content. This observes
terminal presentation, not the display compositor's pixels or live-currentness proof.
Caches are preserved: first useful presentation is never delayed merely to wait for a live
update. The file includes no content, actor,
conversation ID, address, credential or error text.

A failed enqueue records `converse_enqueue_failed`. Normal exit before a useful render
records `stopped_before_useful_render`; unwinding records `panicked_before_useful_render`.
A killed process or failed trace write may leave a partial file. Missing final evidence is
not a latency success. Read/write overhead perturbs the opt-in run. Retain the external
monotonic launch/key times, exit/result, exact installed source/build, host and ordinary UI
context separately; process loading before `main` is outside the internal clock. Do not
equate a subscribe-to-frame `STUI_TIMING_LOG` record with useful presentation.

## Images

Every image in a conversation is a line saying what it is before it loads: a message's
(`▣ name · type · size  open`), a transcript's (`[image type, size]`), and an ask's on its
card. Click the line, or press Ctrl+U on the focused row (Ctrl+↑/↓ move the focus), to load it
inline; again to hide it. Where the terminal draws images (kitty, iTerm2, a sixel terminal,
also through tmux) the first image on screen loads by itself; elsewhere it draws in half
blocks. stui asks the terminal itself, once at start, and asks only a terminal that answered
a status report, so one that answers nothing gets half blocks and no stray bytes.

An image is read, decoded and scaled to at most 1600 pixels on a worker; its terminal
encoding is built off the draw path, an image the edge of the pane cuts draws the rows still
on screen, and its pixels leave memory once it is more than a screen away. An owner that
cannot be reached says so on the image's line, with the way to try again. `open` shows the
image in this machine's viewer, or saves it under its name to `ST3_DOWNLOADS`, else the XDG
download directory, else `~/Downloads`.

## Resource sidebar

In spaces, Ctrl+S shows the resource sidebar beside the panes. It follows the person's
shared Sidebar arrangement: folders in register order, then Everything else grouped
into Agents, Missions, Terminals and Machines. The view flattens nested folders to one
level without changing their registers. Click or Enter opens a resource; dragging a
row out still opens its tab in a pane or split. Folders can be made, renamed and deleted through `arrangement.edit`; the stream
supplies the winning registers after each edit.

Click the filter row or press `/` while the sidebar has focus to narrow it; Esc clears
it. Arrows move through rows and Enter opens a resource or toggles a group. Folder
groups start open. Everything else and its kind groups start collapsed above 30 items;
filtering expands matches. Collapse choices and previously seen resource names are
kept privately on this device, with atomic catalog saves. A missing subject remains
marked unavailable until explicitly cleared; the catalog is never pruned by age. The
terminal catalog reads a 200-item window and displays a notice when more exist.

Fresh workspaces show the host and `Ctrl+K: New terminal`, with a welcome row. An
existing device's explicit sidebar visibility choice is kept. Older daemons lacking
sixteen-subscription capacity keep the three live conversations and terminal; their
sidebar shows grouped resources until the daemon supports the optional window.

With the sidebar focused, `n` names a new folder, `m` on a resource chooses its folder,
`r` renames the selected folder, `d` asks to delete it, and `+` on a folder opens the
same Ctrl+T finder narrowed to resources outside that folder. Selecting a destination
previews the move; `m` commits it. Deletion requires `y`; Enter keeps the folder.
Folder grips reorder folders, and dragging rows moves or reorders resources. Dragging
a space tab onto a folder files its resource while keeping the tab open. Clicks and
Enter still open resources or toggle groups. Position edits include dormant placement
keys, and colliding concurrent keys are rekeyed atomically rather than dropping rows.

The sidebar also offers pull requests, repositories and other observed graph resources using
the existing optional `resources.list` read. Opening the sidebar or changing a tab refreshes
three bounded 200-item windows (all observations, PRs and repositories); a truncation note
names any limit. There is no resource polling or added collection subscription. A connection
without that read keeps the existing browser and shows its reason. Resource tabs show the
observed vocabulary kind and facts; their links use the existing Open link action. The folder
finder can explicitly file a typed graph subject before it appears in a window. Missing
subjects stay marked, and spaces keep their resource tabs.

Opening a resource tab also reads that exact subject with the existing prefix filter so
a catalog limit does not make an available detail vanish. Session and PTY IDs only navigate.

On a member granting `inventory-collections` version 1, the terminal UI follows clients,
the selected usage period, machines, devices, native sessions and terminal lists
through the existing collections socket. Fleet, visible panes, sidebar and
palette demand select the windows. Leaving keeps them for the same thirty-second
grace as missions; switching usage periods immediately replaces the old usage
slot. All sixteen slots remain bounded, including the three live conversations
and selected terminal. Client/usage timers, summary-driven machine rereads and
inventory focus reads are removed. Existing protocol heartbeats and offline
wording remain, and the sixty-second cache save is still a local write.

Older members read inventories when their relevant view opens or its period
changes; Fleet says these inventories are read, not followed. Read errors and
partial windows stay visible. Connection generations fence legacy results, so a
late read cannot replace a newer member's streamed rows. The actual-binary idle
transport test runs `scripts/stui-test-client` with `STUI_TIMING_LOG` on a private
daemon, opening the Fleet list and Usage from Ctrl+K and holding them idle beyond every old
read interval.

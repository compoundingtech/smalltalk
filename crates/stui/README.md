# Smalltalk terminal client

`st` in a terminal opens spaces on the live graph: splits with their own tabs, each showing a conversation,
a mission, a terminal, Home or usage. `st ui --space NAME` opens a named space. `st ui --demo`
shows them on invented data and sends nothing. `st ui --classic` keeps the layout from before
spaces for now; it is going away ([#1166](https://github.com/compoundingtech/smalltalk/issues/1166)).
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

Keys (`?` shows them all): `Ctrl+K` opens anything (agents, missions, machines, spaces, and what
was said in conversations); `Tab` and `Shift+Tab` move between a split's tabs; `Alt+arrows` move
between splits; `Ctrl+T`, `Ctrl+V`, `Ctrl+X` and `Ctrl+W` open a tab, split right or below, and
close a tab; `Ctrl+S` shows the sidebar and `Ctrl+H` opens Home over the space; `Ctrl+Q` quits.
In a conversation, letters type into its message box and commands are chords: `Ctrl+F` finds,
`Ctrl+E` expands tool output, `Ctrl+P` simplifies, `Ctrl+D` shows the agent's details, `Ctrl+]`
attaches the agent's terminal and `Ctrl+\` leaves it. A drag selects text in one pane and copies
it on release. Scrolling to the top of a conversation loads the page before it. Home cards show
their own keys; `y` confirms what a card asks, and Enter never does.

The [terminal tab input reference](../../docs/stui-terminal-tab.md) records the measured
input modes, query replies, and remaining gaps, including the headless PTY probe.

Verification:

```sh
cargo test -p stui --locked --lib
cargo test -p st3 --locked --test integration typed_keys
cargo test -p st3 --locked --test integration client_only
cargo build -p st3 --locked
ST3_PERSON=person/<your-id> python3 crates/stui/tests/pty_smoke.py target/debug/st3
```

The PTY smoke test runs with or without a local daemon. It checks first-frame and key-to-redraw
latency, also against a deliberately stalled getter, plus alternate-screen restoration after
normal exit, SIGTERM, a long-running PTY hangup, and a debug panic. For a packaged release
binary, pass its path followed by `--no-panic`; the release build has no debug panic hook. The two
QA scripts need a live daemon and the `pty` executable: one opens an agent from `Ctrl+K`, scrolls
it and drags to copy; the other attaches the agent's terminal and leaves it by `Ctrl+\` and by a
click on its header. Use a person of their own (such as `person/<qa-name>`) so they mark nothing
read for anyone.

Setup callers can opt into a fresh shell with
`Args::default().with_setup_command("printf setup-ready")?`. This programmatic entry point
stages a nonempty command with no control characters; it never adds Enter. It waits for the
live space and native terminal attachment, and navigation while either is pending cancels
prefill. The command is consumed once and never repeated on reconnect. Paired-device clients
ignore this entry point. Setup fact discovery and Home checklist rows are separate upstream work.

The fixture exercises the real library, an isolated daemon, and a real shell. It checks the
rendered command, an unchanged sentinel while idle, execution only after Enter, and terminal
restoration after leaving the shell with Ctrl+\ and quitting with Ctrl+Q:

```sh
cargo build -p stui --example setup_terminal -j2
python3 scripts/tests/test-setup-terminal.py target/debug/examples/setup_terminal target/debug/st3 "$(command -v pty)"
```

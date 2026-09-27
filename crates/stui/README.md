# Smalltalk terminal client

`stui` connects through the generated Rust `st3.client.v0` client, the same typed data boundary
used by the CLI. It paints immediately, hydrates attention/agents/sessions first, then fills in
mission and fleet details without blocking keys. A private, actor-and-endpoint-scoped read-only
cache keeps the last snapshot visible while reconnecting. Actions still need a live connection
and fresh fences; there is no offline mutation queue.

Install the repo's `.#st3` Nix package and run `ST3_PERSON=person/<your-id> stui` in a terminal
with a running st3 daemon. For source development, use
`ST3_PERSON=person/<your-id> cargo run -p stui --locked`. `ST3_ENDPOINT` can override the discovered Unix socket. When `ST3_PERSON` is unset,
`stui` uses `person` from `~/.config/st3/config.toml` (or `$XDG_CONFIG_HOME/st3/config.toml`).
A concrete person identity is required so Now and devices show the right data.

Keys: `1`–`4` switch Now, Chat, Control, and Fleet; arrow keys or a sidebar click select an item;
the mouse wheel and PageUp/PageDown scroll the detail pane; `End` follows the newest Chat message;
`h` or the History control opens Chat history and details, and its Load older pages control fetches more history;
`s` hides the sidebar; `v` or the Select text control releases mouse capture for native terminal text
selection (press `v` again to return); `i` shows connection details; `q` quits.
Now cards show the available action keys. Choose an action, enter a reason for a review decision,
then confirm with `y`; actions without a typed TUI flow are marked CLI. Chat shows recent received messages for the selected agent and
bounded normalized history for its current session. It preserves message line breaks, simplifies Markdown headings, lists, quotes and code blocks, and hides known internal transcript markup across supported drivers. Running undeclared
harness sessions discovered on the connected host also appear there and in Fleet. An exact native session offers `m` to import it into st3 after `y` confirmation; this stops the exact running process and resumes it under st3. Unresolved processes cannot be imported from the TUI. Their discovery refreshes every
15 seconds even without a graph event. Discovery is local to the connected host, not a claim
that unqueried machines have no undeclared sessions. Press `c` to compose for a managed agent
and Enter to attach its available terminal. Declared agents follow their `under` relationships
in the Chat tree, including st3 descendants under the st3 parent; undeclared sessions remain separate. Active work badges keep top-level agents' work visible. Control groups missions by blocked,
waiting, running, standing, drafts, and archive. It shows progress for the latest run and the
current step, owner, blockers, and next action in the detail pane. A `+` on progress means the
bounded history did not include every step. System missions are hidden initially; `x` toggles them. In Control, `c` starts a
new mission launch. During attach, the
terminal receives every key except Ctrl+backslash, which detaches. The visible Return control
can be focused by clicking it and activated with Enter. On terminals narrower than 66 columns,
the sidebar hides automatically.

Now contains only open attention addressed to the current person. Agent transcript text and
unread messages do not become person attention. Resource lists stop after four pages of 50 items;
the active conversation pages past status-only entries until it has twelve content entries or
reaches four pages. Older loaded messages are shown in the History pane, which can fetch up to
32 pages on demand. Recent messages load only for the selected agent.
Event bursts refresh affected projections; a full refresh runs every two minutes. The
event cursor is bounded and deduplicated; a
cursor gap clears the cached timeline and reloads projections from a fresh capability cursor.

Verification:

```sh
cargo test -p stui --locked
cargo build -p stui --locked
ST3_PERSON=person/<your-id> python3 crates/stui/tests/pty_smoke.py
ST3_PERSON=person/<your-id> python3 crates/stui/tests/interaction_qa.py target/debug/stui
```

The PTY smoke test needs a running local daemon. It checks first-frame and key-to-redraw latency,
also against a deliberately stalled getter, plus alternate-screen restoration after normal exit,
SIGTERM, a long-running PTY hangup, and a debug panic. For a packaged release binary, pass its path followed by `--no-panic`;
the release build has no debug panic hook. Ignored live tests measure first data and full-snapshot
latency.

# Conversation model and terminal drawing

The default build is a renderer-independent model with no ratatui dependency. It provides
timeline updates and older pages (`Timeline`, `Frame`, `Older`), conversation entries and
delivery/markup cleaning (`adapt`, `clean_message_text`), pane state and intents, display-column
selection text, density/fold identities, and serializable style rules. The shared transcript
fixtures in `fixtures/clients/transcripts` are the contract with the phone's TypeScript model.

Exposed status, usage, redaction, truncation and unsupported timeline variants remain visible
as event notices. Unsupported bodies are not dumped. Attachment-only native content preserves
its media type and authorized reference without fetching bytes; graph mail keeps image refs
and labels other media. An unbound transcript is unavailable, not evidence that the harness
has done nothing. Projection availability warnings keep their supplied explanation.
Usage notices show semantics, supplied token counts and supplied cost, omitting nulls and
attribution IDs. Unknown-role content does not expose its text; empty plain content is skipped,
and media references remain visible. Diagnostic messages and unsupported type labels are
bounded Unicode-safe previews. Mail envelopes retain their next content across intervening
status events and emit unpaired media refs only at the next message or end of the window.

```toml
[dependencies]
st3-conversation-ui = { path = "../st3-conversation-ui" }
```

Selections accept strings or any UI line type implementing `SelectionLine`. Its `continuation`
method describes a visual wrap: `Some(" ")` joins at a space, `Some("")` joins inside a word,
and `None` preserves a real newline. The same selection algorithm strips message edges,
code fences and shared indentation for every renderer.

```rust
use st3_conversation_ui::Selection;

let selection = Selection {
    pane: "session/demo".into(),
    anchor: (0, 0),
    head: (1, 40),
};
assert_eq!(selection.text(&["▎ ```sh", "▎   cargo test"]), "cargo test");
```

Terminal consumers opt into `features = ["ratatui"]`. This adds `Cache`, `Theme`, and the
`conversation`, `doc`, `text`, `ansi`, and `theme` modules, plus the `SelectionLine`
implementation for ratatui's `Line`. stui and the st CLI enable this feature explicitly.

Verify the two builds independently, since a workspace build unifies the terminal consumers'
features:

```sh
cargo test --locked -p st3-conversation-ui --no-default-features
cargo test --locked -p st3-conversation-ui --features ratatui
cargo tree --locked -p st3-conversation-ui --no-default-features -e normal
```

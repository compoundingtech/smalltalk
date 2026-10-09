# Developing Smalltalk

## Contributing

Read the [product README](../README.md) and the guide for the behavior you are changing.
Use invented people, host names, and paths in this public repository. Run
`python3 scripts/check-public-repo` before opening a pull request.

For st3 claims, missions, replication, runtime ownership, or recovery, start with the
[documentation index](st3/README.md). For st2 lifecycle, messaging, teardown, or presence,
read and preserve the proofs in [st2 invariants](st2/invariants.md).

## Build from source

From a checkout, install with Nix:

```sh
nix profile install .
```

For local development, enter `nix develop`. The shell provides Rust, sccache,
cargo-nextest, and mold on Linux. Cargo uses mold for Linux links and the system
linker on macOS; dev and test builds keep line tables for workspace crates and
omit dependency debug info. On both platforms the shell sets `RUSTC_WRAPPER` to
sccache. Run tests with `cargo nextest run --workspace --locked`.
Outside the Nix shell, install mold on Linux and cargo-nextest separately; the
repository's `.cargo/config.toml` still selects mold for Linux builds.

Client terminal screens use `pty-terminal` and the same pinned `libghostty-vt` artifact as
the PTY runtime. Styled runs carry terminal-cell widths (including wide characters), soft-wrap
continuations, strikethrough and admitted OSC 8 links; keyboard modes include kitty flags.
The Nix package and developer shell link the shared static library through pkg-config, so
building Smalltalk does not require Zig or a Ghostty source checkout.
The runtime, screen projector and the terminal UI pin their PTY protocol crates to the same
producer revision, keeping one shared protocol source in the workspace.

Nix vendors the PTY Cargo git dependencies from the existing `pty` flake input's
GitHub archive, not an anonymous Git checkout. Vendoring fails during evaluation
if the resolved PTY revision in `Cargo.lock` differs from the flake input, so update
the Cargo dependency pins and the `pty` input together. The input's locked NAR hash
also supplies the Cargo source hash. Any new Cargo git dependency needs an
archive-backed source before Nix can vendor it.

Without Nix, build and install from a checkout with a Rust toolchain and the matching
`libghostty-vt` artifact. Set `PKG_CONFIG_PATH` to its `share/pkgconfig` directory;
`pkg-config --static --libs libghostty-vt-static` must resolve before building:

```sh
scripts/install                  # into ~/.local/bin
scripts/install --bin-dir DIR    # or anywhere else
```

When pkg-config cannot find the artifact, Cargo builds `libghostty-vt` from the pinned Ghostty
source instead, which needs Zig 0.15.2 on `PATH`. On macOS, the vendored
[`libghostty-vt-sys`](../vendor/libghostty-vt-sys/README.md) build script lets Zig 0.15.2 link against
the macOS 26.5 and 27 SDKs.

The script builds and installs `st3` and `st3-migrate`, and makes `st` a symlink to
the installed `st3`. `st` is never a separate build. On macOS, st3 lives in a fixed app bundle; see [macOS installation and signing](st3/macos-installation.md). A source install also needs [`pty`](https://github.com/compoundingtech/pty-rust) on `PATH`.

## Continuous integration

Workspace CI runs on pull requests and merge groups. The five required checks are `linux-gate`,
`isolation-vm`, `genie-freshness`, `typescript-client`, and `mail-redelivery-canaries`.
A ready pull request lands through GitHub's merge queue:

```sh
gh pr merge NUMBER --auto
```

The queue checks the exact commit that lands. Main upkeep verifies those checks, runs `perf-cost`,
and fills missing main caches. The primary Linux test job uses our self-hosted ci1 runners, with
Namespace as overflow for ordinary PRs. The second test shard and supporting jobs use Namespace. Trusted PRs labelled `ci-priority` and their queue entries use the reserved
`ci1-priority` lane; other merge groups use `ci1-merge`. Forks run on Namespace.
macOS CI is currently disabled; its retained workflow is not a required check.
See [CI operations](ci.md) for stage scope, routing, caches, and failure inspection.

## Shared UI models

[Build your own client](clients/build-your-own.md) maps the Rust and TypeScript pieces
and includes a small [example TUI](../examples/client-tui/) built and tested in CI.

`crates/st3-ui-model` provides renderer-independent UI semantics without a ratatui dependency.
Its broad name is intentional; initially it contains only the terminal UI's mission model (`Word`,
`StepState`, `Mission`, `Step`) and mission derivation. The terminal UI consumes that same model.
`missions::adapt` borrows typed mission and agent projections plus unresolved, actor-filtered
attention. Callers supply the current time and display policies explicitly; collection loading,
clocks and application naming remain outside the crate. Mission precedence, queue/keep-open
rules, outcomes and rich step details retain the terminal UI's existing behavior.
Adapted steps retain their stable step-run `id` and claimant-or-assignee `seat` (absent for
agentless steps), so consumers can select duplicate paths across open runs and navigate to
the actual execution seat without reconstructing identity from display labels.
`Mission.step_metadata` retains each selected step's raw `blocked_reason` and `last_progress`,
keyed by its step-run ID. These facts survive state changes independently of a step's waiting
blocker or derived queue `note`, and are not normalized by display text policies.
Shared widgets and other UI models are separate follow-up work, not part of this mission extraction.

## Embedding native conversations

`crates/st3-conversation-ui` provides the same native conversation presentation used by
`stui`, without terminal acquisition, application tabs or daemon connections. Feed
`st3-client` conversation frames into `Timeline::apply(Frame { replace, has_more, items })`,
adapt the timeline with caller-owned display names, and render it with `Cache::render`
and caller-supplied `Theme` tokens. The returned document contains styled lines and typed
`PaneIntent` targets. `State` retains scrolling, tool expansion, display-column selection
and composer drafts; send, open and older-history intents are executed by the embedding app.
Replacement frames remove the previous bounded window; incremental frames revise entries
by ID. Frame `has_more` is `Option<bool>`: an absent delta leaves the timeline's availability
unchanged, while an explicit value updates it. A replacement or changed session starts fresh.
The timeline keeps a boolean for the current window, and older pages track their own start.

The shared crate and the `stui` library use workspace Ratatui 0.30. An embedding application must align its
rendering dependency before passing buffers or lines across this boundary. Native conversations are not a terminal emulator: harness menus and arbitrary
permission prompts still require access to the harness terminal.

See the [conversation crate](../crates/st3-conversation-ui/README.md) for model-only and
Ratatui builds, and [build your own client](clients/build-your-own.md) for the client contract.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/st3` | Current daemon and CLI. |
| `crates/stui` | Terminal UI library used by `st`. |
| `crates/st-drivers`, `crates/st-runtime` | Harness drivers and shared runtime. |
| `crates/st3-client`, `crates/st3-feed`, `crates/st3-schema`, `crates/st3-client-codegen` | Client API, feeds, schema, and code generation. |
| `crates/st3-ui-model`, `crates/st3-conversation-ui` | Shared mission and conversation presentation. |
| `clients`, `apps/ios` | TypeScript/Swift clients and the iOS app. |
| `crates/st3-migrate` | Migration tooling. |
| `src`, `tests` | Legacy st2 package and its integration tests. |
| `components`, `evals`, `fixtures` | Provider components, evals, and proof fixtures. |
| `examples/st3`, `docs` | Runnable declarations and documentation. |
| `nix`, `flake.nix`, `scripts`, `.github` | Packaging, developer tools, and generated CI workflows. |

The `stui` package is a library embedded in `st`; it has no standalone executable. Its existing
Clippy baseline allowances remain under the `stui` package key because the library still owns
the renderer and its existing lint baseline.
Removing the binary does not rename the library or the protocol client identity.

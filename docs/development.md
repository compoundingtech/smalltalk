# Developing Fractal

## Contributing

Read the [product README](../README.md) and the guide for the behavior you are changing.
Use invented people, host names, and paths in this public repository. Run
`python3 scripts/check-public-repo` before opening a pull request.

For st3 claims, missions, replication, runtime ownership, or recovery, start with the
[documentation index](st3/README.md). For st2 lifecycle, messaging, teardown, or presence,
read and preserve the proofs in [st2 invariants](st2/invariants.md).

## CLI actor contract

`st actor-contract --json` prints the actor syntax of the binary that runs it. It
does not read config or call a daemon. The versioned `contract` has one entry for
each callable clap command path, including hidden commands and optional parent
commands. `aliases` lists other names under each canonical command. `contract_sha256` is
the SHA-256 of compact JSON for `contract`, not of the formatted output. The
printer computes this digest; the checked-in snapshot omits it and the equality
test compares the contract body. The printed `actor_semantics` reminder comes
first: actor: none means no --as syntax.

Version policy:

kind st3.actor-contract.v1 and version 1 stay for: new commands, new aliases, new optional fields on an entry, and new values of an existing enum (actor, and any later enum). Readers MUST ignore unknown fields and MUST treat an unknown enum value as unknown, never as one of the values they know; they must not infer --as syntax, person support or authority from an unknown value. kind and version bump together for: a removed or renamed field, a changed field type, a removed enum value, or a changed meaning of an existing field or value. contract_sha256 changes with ANY content change (a command moving class included) and identifies one binary build; it is not a version. The contract describes argv syntax and is never an authorization input.

No actor-contract reader exists in this repository yet; the unknown-enum rule
is a requirement on future readers. Existing tests parse JSON only to check the
producer's output and snapshot, not to interpret unknown actor values.

`actor` means whether argv accepts `--as`: `none`, `optional`, or `required`.
`actor_environment` names an environment default, if one exists. `accepts_person`
means the real argument value parser accepts `person/NAME`; it does not grant
person authority. An optional flag can still need an actor from config or the
environment at run time. A command with no flag can still use a configured
identity. `can_carry_person` records whether a command can carry a person actor,
including configured-person fallbacks and the separate `note --actor` option.
This does not mean an agent may use that person: identity guards and daemon
authorization still apply. `as_role` distinguishes an actor from the document
name in `documents put --as` and the ignored compatibility flags on reads.
`ignores_actor` identifies those ignored flags. Do not use this contract as an
authorization decision.

Add `--effective` to include only the selected `instance`, `config_path` and
`state_dir` metadata; no config values are printed. Instance activation and the
existing config loader determine the state directory. **`config_path` always
reports `Config::default_path()` for the selected instance, not the actual path
of a custom config file.** Do not use it as evidence of which file was loaded.
These fields are outside the hashed contract and can differ between calls.
Without `--effective`, static mode reads no config (even an invalid config is
irrelevant) and does not contact a daemon.

After a deliberate CLI change, review the changed actor entries and regenerate
the checked-in contract:

```sh
st actor-contract --json | jq 'del(.contract_sha256)' > docs/st3/actor-contract.json
cargo nextest run -p st3 -E 'test(actor_contract)'
```

Use the binary built from the changed source. Resolve any rebase conflict in
`docs/st3/actor-contract.json` by running that regeneration command, never by
hand-merging the JSON. The snapshot test fails if a
command is added or removed, if its actor parser changes the published fields, or
if an alias changes. Supplement coverage also compares the reviewed setup/UI,
note and changes paths, including every descendant, against the real clap
inventory. Additions or removals require reviewing the identity handlers named
beside `implicit_person`; mutation tests exercise added setup/UI descendants and
a removed UI descendant. `st ui open` resolves the configured person before
queueing a request in that person's terminal UI request directory.
This is freshness coverage, not proof of live runtime fallback or authority.
Host, account, repository and harness policy remains in the
operator layer. Person-authority enforcement is a separate daemon change.

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
The default and perf shells can opt into an owner-provisioned read-only WebDAV
cache with `ST_SCCACHE_READ_ENDPOINT` and `ST_SCCACHE_READ_SERVER_PORT`. Leave
both unset for the existing cache behavior. The port must be separate from the
current sccache server; the owner must qualify it as unused before the first build.
This does not change a running server's configuration or grant network access.
Provide an actual read-only service credential through sccache's authentication
environment variables. The shell sets `SCCACHE_WEBDAV_RW_MODE=READ_ONLY`, but the
service must independently reject writes. Never reuse a trusted CI writer identity
in a builder or an untrusted job. Existing explicit cache configurations cannot be
combined with this opt-in. See [compiler-cache measurement](ci.md#compiler-cache-measurement).
Outside the Nix shell, install mold on Linux and cargo-nextest separately; the
repository's `.cargo/config.toml` still selects mold for Linux builds.

Client terminal screens use `pty-terminal` and the same pinned `libghostty-vt` artifact as
the PTY runtime. Styled runs carry terminal-cell widths (including wide characters), soft-wrap
continuations, strikethrough and admitted OSC 8 links; keyboard modes include kitty flags.
The Nix package and developer shell link the shared static library through pkg-config, so
building Fractal does not require Zig or a Ghostty source checkout.
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

## Workspace dependency licenses

The root pnpm workspace includes the iOS app and TypeScript clients.
`buck2/dependencies/licenses.json` records every name/version in the lockfile's `packages`
set, not just the narrower Buck dependency closure. The inventory is generated, including
font licenses such as `MIT AND OFL-1.1`; do not edit its entries by hand.

Regenerate after changing the lockfile:

```sh
nix develop .#web -c pnpm install --frozen-lockfile
nix develop .#web -c python3 scripts/ci-fractal-web-licenses
```

The generator reads installed package manifests, including nested versions in the hoisted
install, using `license` and falling back to legacy `licenses`. Missing installed packages
are reported explicitly and resolved from their exact npm tarballs, verified against the
lockfile's SHA-512 integrity before reading `package.json`. This covers optional binaries
for other platforms without substituting registry metadata, hand-maintained exceptions or
`UNKNOWN`. Generation and full checks need network access for any missing packages.
Missing license declarations or conflicting installed copies fail closed.

Two CI guards use the same script:

```sh
# No node_modules or network: exact package-set coverage and lockfile/provenance drift.
python3 scripts/ci-fractal-web-licenses --check-lockfile
# After a frozen install: exact inventory content versus package manifests.
nix develop .#web -c python3 scripts/ci-fractal-web-licenses --check
```

`genie-freshness` (`scripts/ci-genie-freshness`, also built as `nix build .#checks.<system>.genie-freshness`
in the Nix sandbox) runs the lock-only guard and the companion
`python3 scripts/ci-fractal-web-licenses-test`. The fractal-web execution lane runs the
full guard immediately after its frozen install. The generator/full guard restore the
inventory's read-only permissions (Git does not preserve them).

## Fractal web

[Fractal web implementation](fractal-web.md) documents the app’s security policy, transcript paint,
captured changes, composer, arrangements, roster and terminal attachment.

## Continuous integration

Workspace CI runs on pull requests and merge groups. The five required checks are `linux-gate`,
`isolation-vm`, `genie-freshness`, `typescript-client`, and `mail-redelivery-canaries`.
A ready pull request lands through GitHub’s merge queue after review and all five required checks
pass. Keep the approved head fixed; a new head requires review of its own changes before queueing.

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
`st`’s terminal interface, without terminal acquisition, application tabs or daemon connections. Feed
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

## Fractal web changelog

Author a short user-facing entry in `apps/fractal-web/.changelog/YYYY-MM-DD-slug.ts`.
That one new file is the whole change: never edit `mod.ts`, which derives the registry from
the directory through `registry.generated.ts`. Git ignores that generated file (a committed
sorted list conflicts whenever two branches add an entry), and every build, typecheck, test and
dev-server start regenerates it; `pnpm --filter fractal-web changelog:registry` does so by hand
and `changelog:registry:check` fails when it is missing or stale. Export `{ title, summary, kind?,
publicPrUrl? }` as the default value. An entry is public unless it also exports
`visibility = 'private'`, which a public build rejects.
Summaries contain one to three complete sentences; kinds are
`feature`, `improvement`, or `fix`. The immutable filename supplies the entry ID and date.
Only link to a confirmed public GitHub pull request. Do not put private names, hosts,
paths, seats, or links in public entries.

The existing extension build hook compiles the strict versioned manifest before bundling,
using the exact shared build identity. Public builds read only the public registry; private
overlay builds combine the inventories and badge private entries. The compiler checks the
registry/file inventory and deterministic newest-first ordering. A public build rejects private
entries and private source imports; the UI is not the privacy boundary. The emitted
`changelog.v1.json` describes precisely the entries included in that build.

The Changes disclosure lazily loads prose and acknowledges included IDs on opening. The
device-local cursor is scoped by origin, product, and deployment channel; `live`, `main`,
and `stable` do not share cursors. Seen IDs survive rollbacks and skipped builds. No version
string or timestamp comparison decides whether a change is new. First visits show “Recent
changes”; subsequent visits show “New since you last looked” followed by “Recent”.
The Changes trigger mounts next to the build version in the existing status bar.
Its React Aria popover anchors to the visible bar with an 8px gap and restores focus
on dismissal. It uses the shared light/dark theme, glass surface, and change-kind pills.
The contract and compiler do not depend on a status-bar package or disclosure slot.
The button names unread changes directly for assistive technology; its dot is decorative.
Popover dimensions and spacing use composition tokens. Story fixtures are synthetic, and
already-open story disclosures seed the acknowledged cursor while retaining the opening baseline.

The `fractal-changelog` PR check requires a newly added entry for product UI source changes
under `apps/fractal-web` or `packages/fractal-ui`. The `no-changelog` label explicitly opts
out. Run its dependency-free policy tests with
`node --test apps/fractal-web/scripts/ci/changelog-policy.unit.test.mjs`.
Like the other PR jobs, this check allocates no runner for drafts and starts when
the PR becomes ready for review. Label additions and removals refresh the opt-out check.
This narrative inventory is separate from operational `release-notes/`; keep the public
wording consistent when both describe the same change.

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

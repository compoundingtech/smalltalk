# st3 scenarios specification

This document specifies `@smalltalk/st3-scenarios`: one package that holds the scenario kit
(seeded factories, slices, worlds, replay, React and Storybook bindings) and the world catalog.
Every Fractal client uses it for realistic, coherent, privacy-safe demo and test data.

## Status

Draft. Implementation follows this document.

## Scope

Defines: the data model (factories, slices, worlds), the time model, the committed fixture
format, recordings from a scratch daemon, the replay transport, the React and Storybook bindings,
the world catalog, the CI gates and the consumer contracts.

Does not define: the client-v0 wire contract (see
[docs/st3/client-v0/README.md](../../../docs/st3/client-v0/README.md) and
[collections.md](../../../docs/st3/client-v0/collections.md)), what a client shows (see
[docs/clients/ui-contract.md](../../../docs/clients/ui-contract.md)), or consumer component props.

The client-v0 golden fixtures in `docs/st3/client-v0/fixtures/` stay the normative contract
examples. Scenarios are product-shaped data built on that contract; they do not replace it.

## Decisions

These decisions are fixed for this package. Sections below cite them as D1–D10.

- **D1 Home:** one public package, `clients/typescript/st3-scenarios`, holds the kit and the worlds.
- **D2 Consumers:** fractal-ui Storybook, fractal-web (tests, dev mode, e2e), stui, the fractal
  TUI and iOS. stui's `fixtures/clients/demo-world.json` becomes one generated world.
- **D3 Realism:** seeded synthetic worlds plus real client-v0 frames recorded from a scratch daemon
  that drives a scripted fake harness. No capture from a live host enters the repository.
  Scenarios read like real engineering work.
- **D4 Gates:** CI decodes every emitted scenario with the real client-v0 codecs in strict mode and
  runs a privacy scan.
- **D5 Time:** every time is an offset from the load-time `now`. Screenshots pin `now`.
- **D6 Artifacts:** generated JSON is committed under `fixtures/scenarios/<world>/<slice>.json`,
  one file per slice with no manifest or shards. A freshness check fails on drift. Rust and Swift
  read the plain files. `huge` commits its full 1,000-agent roster and a bounded conversation
  history window; replay deterministically generates older history from seed metadata.
- **D7 Model:** factories → slices → worlds. Factories are seeded and schema-typed (`agent`, `turn`,
  `toolCall`, `diff`, `terminalRun`); terminal streams use the asciicast v2 shape. Slices are
  `roster`, `conversation`, `sync`, `terminal`, `details` and `attention`. A world composes slices
  over one cast; `world.with({ slice: override })` replaces one slice.
- **D8 Binding:** the kit is Effect-free. Components get props through `useScenarioSlice`.
  App-level stories and fractal-web dev/e2e replay client-v0 frames over a mock WebSocket through
  the real data layer. stui, iOS and the TUI read JSON or use the same replay.
- **D9 Catalog:** seven narratives and the edge states listed under [Catalog](#catalog-d9);
  failed sync has one world per cause.
- **D10 Storybook:** a global Scenario toolbar selects the world. Stories declare
  `parameters.scenario.slices`. Controls override one slice; fixed stories pin a world. Deep links
  use `?globals=scenario:<world>&args=<slice>:<variant>`. Explorer pickers (rendering) stay
  separate from scenario selection (data). Every story renders scenario data; no hand-typed
  lorem fixtures.

## Package layout (D1)

```
clients/typescript/st3-scenarios/
  spec.md                 this document
  README.md               usage
  package.json            plain manifest, like st3-client and st3-views
  tsconfig.json
  vitest.config.ts        pins the test root to this package
  src/
    index.ts              catalog, loadWorld, types           (no React, no Effect)
    clock.ts              manualClock, realClock (shared by ./replay and ./react)
    kit/                  rng, time, cast, vocabulary, factories/, asciicast, screen, slice, fold, world
    worlds/               one module per world
    replay/               createReplay, clocks, frame materialization, mock socket and fetch
    react/                ScenarioProvider, useScenarioSlice  (React peer)
    storybook/            globalTypes, decorator, argTypes, parameter types (React peer)
  recordings/             committed curated recordings + their fake-harness scripts
  scripts/
    emit.ts               writes fixtures/scenarios; --check compares bytes
    decode.ts             strict decode gate (imports @smalltalk/st3-client/schema)
    scan.ts               privacy gate
  test/                   vitest unit and property tests
fixtures/scenarios/
  index.json              catalog: worlds, titles, slices, cast
  <world>/<slice>.json    one file per world and slice
```

Package name: `@smalltalk/st3-scenarios`, `private: true`, `type: module`. Like
`@smalltalk/st3-client` and `@smalltalk/st3-views`, it exports TypeScript source with explicit
`.ts` import specifiers.

| Export | Contents | May import |
| --- | --- | --- |
| `.` | catalog, `loadWorld`, world and slice types, factories | `@smalltalk/st3-client` types only |
| `./replay` | `createReplay`, clocks | `.`, `@smalltalk/st3-client` (`applyWindow`, types) |
| `./react` | `ScenarioProvider`, `useScenarioSlice` | `.`, `react` |
| `./storybook` | `scenarioGlobalTypes`, `withScenario`, `scenarioArgTypes`, types | `./react` |

Dependencies:

- Runtime: `@smalltalk/st3-client` (workspace). `react` is an optional peer, needed only by
  `./react` and `./storybook`. No Effect, no faker, no MSW.
- Development: `effect` (pinned with the client, for the decode gate only), `typescript`,
  `vitest`, `@effect/vitest` (property tests).

An import check fails when any file reachable from `.`, `./replay` or `./react` imports `effect`
(D8).

The package has no Buck target yet: Buck builds only importers whose closure the Buck lock
carries, and that closure has no React or Storybook. Like `packages/fractal-ui`, its typecheck and
tests run as lanes of `scripts/ci-fractal-web`. A Buck target joins when the Buck importers
include a React package.

## Model (D7)

```
cast (one per world: agents, people, hosts, missions, repositories)
  │
  ├─ factories  agent · turn · toolCall · diff · terminalRun    seeded, return client-v0 wire values
  │
  ├─ slices     roster · conversation · sync · terminal · details · attention
  │               each = state at now + timeline of later events; has named variants
  │
  └─ world      id, title, narrative, cast, seed, one value per slice
                  world.with({ conversation: 'long' })  → same cast, one slice replaced
```

### Randomness

`kit/rng.ts` is a small seeded generator (sfc32). Each factory call takes the generator and
derives a child generator from a stable label (`fork(rng, 'agent/3')`), so adding a call in one
place does not shift values elsewhere. `Math.random` and `Date.now` are forbidden in `src/kit`
and `src/worlds`; a unit test checks this. The same seed always produces the same bytes.

### Cast and vocabulary

A world first draws its cast: agents with roles, harnesses, models, hosts and worktrees; people;
missions with steps; repositories with files. Every slice of the world references cast members, so
the agent that is blocked in `roster` is the same agent whose `conversation` shows the failing
typecheck and whose `attention` card asks for review.

`CastSpec` supplies exactly one of `roles` (the role is also the stable key) or `agents`:
`{ key, role, name?, workspace?, branch? }[]`. Keys are unique lowercase ASCII slugs;
repeated semantic roles use distinct keys such as `builder-1` and `builder-2`. `CastAgent.key`
drives identities, seeded forks, runtime/session/terminal IDs and factory entry/call IDs;
`role` chooses semantic vocabulary. Thus scale casts do not alias agents. Explicit display
names may contain Unicode. Workspace overrides remain beneath `~/src/<project>/` and cannot
contain a `..` segment; branch overrides change display/worktree context, not identity.

Text comes from curated vocabulary banks in `kit/vocabulary/`: task titles, commit messages, file
paths, compiler errors, test names, review comments, shell commands and their output. The banks
describe everyday engineering on invented projects ("rename `fetchUser` to `loadUser` across the
monorepo", "bisect the red `main` after the lockfile bump"). Factories combine bank entries; they
never generate lorem text.

### Identity namespace

Every identity is invented and follows the repository's public conventions:

| Family | Form | Example |
| --- | --- | --- |
| agent | `agent/example/<project>/<role-key>` | `agent/example/atlas/builder-1` |
| person | `person/<name>` from the reviewed invented list in `scripts/check-public-repo` | `person/ada` |
| host | `host/<invented-word>` | `host/harbor` |
| mission, run, step | `mission/example/<project>/<slug>`, `mission-run/…`, `step-run/…` | `mission/example/atlas/store-move` |
| machine | `machine/<host word>` | `machine/harbor` |
| per-agent runtime, session, terminal; fleet | `<family>/example-<project>-<role-key>`, `fleet/example-<project>` | `runtime/example-atlas-builder-1` |
| others | `<family>/scenario-<world>-<n>` | `message/scenario-fleet-mid-refactor-12` |

Paths start with `~/src/<project>`; no absolute home path appears. A display name is distinct
from its identity (`Atlas Builder` over `agent/example/atlas/builder`).

### Factories

Each factory takes a generator, the cast and optional overrides, and returns values typed by
`Models.generated.ts`. TypeScript checks the shape; the decode gate checks the values.

| Factory | Returns |
| --- | --- |
| `agent` | `Agent` resource with joined `current_work`, `next_work`, `upcoming_work`, `subagents`, and its `Runtime` when it runs |
| `turn` | `TimelineEntry[]` for one exchange: the person's or mail message, assistant content, tool calls with results, a final status or usage entry |
| `toolCall` | a `tool_call` entry and its `tool_result` by call id, outcome `ok`, `error` or pending (no result) |
| `diff` | a `toolCall` whose call edits a file and whose result carries a unified diff of the edit |
| `terminalRun` | an asciicast v2 recording of one command and the `TerminalScreen` values a client sees for it |

Each entry gets increasing `sequence`, `revision` 1 unless revised, and a timestamp from the
time model. A streaming entry is emitted as revisions with `final: false` until its last revision.

### Slices

A slice holds the state at `now` (offset 0) and a timeline of events after `now` (offset > 0).
Events at or before `now` are folded into the state. State values are client-v0 wire values.

A timeline event is **frame-shaped**: it groups exactly the changes one server frame or one HTTP
response carries. A synthetic event usually holds one change; an event imported from a recording
keeps the recorded grouping, so page and revision boundaries survive (see
[Recording fidelity](#recording-fidelity)). Each event also carries `store`, the store version
after it (see [Store versions](#store-versions)).

| Slice | State | Timeline events |
| --- | --- | --- |
| `roster` | `agents: Agent[]`, `runtimes: Runtime[]`, `machines: Machine[]`, `order: Id[]` | `changes {upserts, removes, order?}` |
| `details` | `missions: Mission[]`, `work: Work[]` (joined as list reads return them) | `changes {upserts, removes, order?}` |
| `attention` | `attention: Attention[]`, `messages: Message[]` (sources the cards load) | `changes {upserts, removes, order?}` |
| `conversation` | per agent: `session_id`, `items: TimelineEntry[]`, `page_size`, `has_more` | `thread-create {thread}`, `thread-remove {agent}`, `entries {agent, items}` (append or revise), `replace {agent, session_id, items, has_more}` |
| `terminal` | per terminal: owner, its terminal `runtime` (the `Runtime` that `runtimesGet` returns before every attach), `incarnation`, `cast` (asciicast v2), `screens: { at_ms, screen }[]` | `terminal-create {record}`, `terminal-remove {terminal}`, `screen`, `unavailable`, `end`, `incarnation` |
| `sync` | `capabilities` envelope, the [sync script](#sync-matrix) and the `expected` sync status per surface | see [Replay dispatch](#replay-dispatch) |

Creation events carry the complete thread or terminal record, including pagination or runtime,
cast and screens metadata. Resources are genuinely absent before creation, rather than hidden
empty records. Creating an existing key replaces its record; removal deletes membership.
Ready conversation subscribers receive a replacement frame and stay bound to their agent
across a changed session ID. A replaced terminal refreshes the current screen when its
incarnation matches, or rejects the old subscriber with `stale-fence` when it changes.
Membership removal refuses matching subscriptions even when their first reply is held/loading.
All four membership events advance the store. `terminalRecord(ctx, member, run, startedMs)`
builds a terminal runtime and retains only run screens at offsets at or before zero; authors
put later screens in timeline events.

Roster, details and attention may additionally hold `resources: RawResource[]`: future resource
kinds, preserved as encoded `{id, kind, revision, updated_at, ...}` values outside known-kind
dispatch. Unknown `changes.upserts` fold into this collection and `removes` deletes them by ID.
Replay includes them in that slice's primary collection (`agents`, `missions`, `attention`),
its bounded socket window, primary collection HTTP pages and generic resources HTTP observations
without dropping their fields. Every client-v0 known kind, including kinds this kit does not
model in a slice, still requires its proper collection; unsupported known kinds fail the fold.
Their schema-declared header timestamps
participate in `times`; opaque future payload timestamps do not.

`selector` names a subscription by its collection and filter, for example
`{ collection: 'conversation', conversation: 'agent/example/atlas/builder' }`.

Every slice kind has named **variants**. A variant regenerates that slice over the world's cast,
so an override never introduces strangers. Variants per slice:

| Slice | Variants |
| --- | --- |
| `roster` | `default`, `empty`, `loading`, `one-agent`, `all-states`, `huge` |
| `details` | `default`, `empty`, `loading`, `stalled`, `failed` |
| `attention` | `default`, `none`, `one-of-each-kind`, `many` |
| `conversation` | `default`, `empty`, `loading`, `streaming`, `long`, `huge`, `tool-heavy`, `failed-tools`, `remote-only-mail` |
| `terminal` | `default`, `none`, `running`, `unavailable`, `exited`, `restarted` |
| `sync` | one variant per row of the [sync matrix](#sync-matrix) |

`world.with({ conversation: 'long' })` returns a new world with that variant. A world may also
pass a slice value built by its own factories: `world.with({ sync: customSync })`.

Generic tables live in `src/kit/variants/<slice>.ts`, each exporting
`<slice>Variants: VariantTable['<slice>']`. `src/kit/variants.ts` only composes these tables and
reexports sync helpers. Variants synthesize missing default records from the declared cast:
an empty conversation variant can create cast-owned empty threads, and populated terminal
variants can build a cast-owned terminal. A populated variant requires an appropriate cast
member; it never invents a foreign identity or dereferences a missing first record.

`roster.all-states` cycles the supported lifecycle/harness combinations over the declared cast;
it does not add agents merely to display every state. `conversation.long` contains 80 exchanges
per cast agent, and `attention.many` contains 50 cards distributed over cast-owned work.
`conversation.remote-only-mail` uses another cast agent, preferring a different host; a singleton
cast uses agent-to-agent loopback mail, which the protocol permits, never person-origin content.
`conversation.huge` gives the first cast agent a logical 10,000-turn history, four entries per
turn (40,000 entries). It commits only the newest 400 turns (1,600 entries), including the
50-entry live window; a scripted consumer reaches older pages. `roster.huge` retains the full
1,000-agent roster in the normal roster slice, not merely a subscription window.

### Terminal data

`terminalRun` writes the stream in asciicast v2 shape, embedded as JSON:

```json
{ "header": { "version": 2, "width": 100, "height": 30, "title": "cargo test" },
  "started_at_ms": -42000,
  "events": [[0.12, "o", "$ cargo test\r\n"], [1.8, "o", "\u001b[32mok\u001b[0m\r\n"]] }
```

The header omits the absolute `timestamp`; `started_at_ms` places the cast in world time
(D5). `kit/asciicast.ts` exports a cast as NDJSON `.cast` text for asciinema-compatible players.

A synthetic run produces line-oriented output only: text, `\r\n`, `\r` and SGR styles.
`kit/screen.ts` turns such a cast into `TerminalScreen` values with a minimal line model.
Full-screen programs (cursor addressing, alternate screen) come only from recordings, where the
daemon itself produced the screens. This keeps a terminal emulator out of the kit.

## Time (D5)

### Anchor and offsets

- Every committed file is written at one fixed anchor, `2030-01-01T12:00:00.000Z`, which stands
  for `now`. Past instants lie before it; future instants (lease expiry, scheduled reconnects,
  timeline events) lie after it.
- Kit-only fields are offsets and never rebased: timeline `at_ms`, terminal `started_at_ms`, and
  asciicast event times (seconds from the cast start).
- `loadWorld(id, { now })` defaults `now` to the load time. Screenshots and snapshot tests pin it:
  Storybook uses the `scenarioNow` global, Playwright installs its clock, and tests pass `now`.
  With `now` equal to the anchor, loaded values equal the committed bytes.

### Timestamp locations

Only schema-declared instants move. The client-v0 schema marks them: `$ref: Timestamp`
(`x-st-codec: timestamp`, an RFC 3339 string) and `x-st-codec: epoch-ms` (an integer). Durations
(`duration-ms`, `duration-s`) and every other value are never shifted.

- `scripts/emit.ts` walks each wire value against its schema definition and writes the JSON
  pointers of every instant it contains into the slice file's `times` list, with the codec of
  each pointer (`timestamp` or `epoch-ms`). The pointers cover `state` and `timeline`.
- A reader shifts exactly the listed locations by `now − anchor`. It needs no schema and no pattern
  matching, so Rust and Swift readers stay small.
- Content is opaque: message text, tool output, terminal screens, titles and ids never move, even
  when a string looks like a timestamp. Authored content therefore never states an absolute
  date or clock time. A vocabulary rule rejects date and clock patterns in bank entries; relative
  phrases ("about an hour ago") are allowed and stay as written.

### Precision and range

- One canonical precision: milliseconds, as the server emits (`YYYY-MM-DDTHH:MM:SS.sssZ`).
  Emitted `timestamp` values always have exactly three fractional digits; `epoch-ms` values are
  integers. Imported recordings with finer precision are truncated to milliseconds on import.
- `now` is an integer number of epoch milliseconds; a finer clock is truncated.
- Every shifted instant stays in `0001-01-01T00:00:00.000Z`..`9999-12-31T23:59:59.999Z` (and, for
  `epoch-ms`, within the safe-integer range). A `now` that would move any listed instant outside
  this range is rejected with an error naming the pointer.
- Readers accept any RFC 3339 precision on input and **truncate it to milliseconds first**, then
  shift and write exactly three fractional digits. Differences are exact relative to the
  truncated instants: `shift(a) − shift(b) = trunc(a) − trunc(b)`. Two inputs that differ only
  below a millisecond become equal. Emitted files never contain such inputs.

### Shared vectors

`fixtures/scenarios/_vectors/rebase.json` holds cases of `{ input, times, anchor, now, expected }`.
They cover: second, millisecond and microsecond inputs side by side (including two microsecond
inputs in the same millisecond, which become equal); a `now` with nonzero
milliseconds; `epoch-ms` integers; content strings and ids equal to a timestamp (unchanged);
durations (unchanged); range errors. The TypeScript kit, the Rust reader in `crates/st3-client`
and the Swift reader in `St3Client` each run all vectors in their own tests.

## Committed fixtures (D6)

`fixtures/scenarios/<world>/<slice>.json`:

```json
{
  "format": "st3.scenario.slice.v1",
  "world": "fleet-mid-refactor",
  "slice": "roster",
  "variant": "default",
  "anchor": "2030-01-01T12:00:00.000Z",
  "source": { "_tag": "synthetic", "seed": 1 },
  "decode": "strict",
  "loading": false,
  "times": [
    { "pointer": "/state/agents/0/since", "codec": "timestamp" },
    { "pointer": "/timeline/0/upserts/0/updated_at", "codec": "timestamp" }
  ],
  "state": { "agents": [], "runtimes": [], "machines": [], "order": [] },
  "timeline": [{ "at_ms": 4000, "_tag": "changes", "upserts": [], "removes": [] }]
}
```

- `source` is `{ _tag: 'synthetic', seed }` or `{ _tag: 'recorded', recording }`.
- `loading: true` marks a `loading` variant: replay withholds every first reply of the slice's
  subscriptions.
- `decode` is `strict` except in deliberately contaminated slices, which declare `tolerant`
  and `unknown: { pointer: string, known_value?: unknown }[]`. Pointers are exact, unique,
  nonoverlapping JSON pointers inside a wire value. `known_value` is a strict-valid repair
  witness (a known enum, or a complete known resource/entry for a future kind); omit it only
  to remove an extra object key. Witnesses are diagnostic metadata, never transport data.
  Known-object ancestors cannot serve as declarations for contaminated descendants: declare
  each enum leaf or actual excess key. Whole-resource or whole-entry witnesses are permitted
  only when the real tolerant codec identifies their union discriminator as genuinely unknown.
  The gate first decodes the original tolerantly, then strict-decodes the value with all
  declarations repaired, rejecting undeclared contamination. It also strict-decodes each
  declared path in isolation with all other declarations repaired and requires failure.
  This isolates strict-enum root-only diagnostics without matching error strings. Invalid
  paths, stale declarations and invalid witnesses fail the gate. Absent metadata is omitted
  from emitted files, preserving the bytes of uncontaminated worlds.
- `fixtures/scenarios/index.json` lists every world: id, title, narrative, slices, cast (names
  and ids) and seed. Clients build world pickers from it.
- Files have sorted keys, two-space indentation and a trailing newline, so diffs stay readable.
  `fixtures/scenarios/**` is marked `linguist-generated` in `.gitattributes`; `huge/**` is also
  `-diff`.
- Only each world's default slices are committed. Variants are generated in process; the decode
  gate still checks every variant of every world (D4).
- The format remains one JSON file per slice: no per-world manifest, page files or shards.
  `huge` stores all 1,000 agents in its normal roster slice. Its first cast agent's conversation
  has 10,000 logical turns (40,000 entries), but the conversation slice commits only the newest
  400 turns (1,600 entries, sequences 38,401–40,000), including the 50-entry live window.
  The thread advertises `page_size: 50` and `has_more: true`.
- Optional `ConversationThread.history` describes seeded history rather than embedding every
  older entry: `{ kind: 'seeded-turns', seed: number, world: string, total_turns: number,
  total_entries: number, committed_from_sequence: number, next_cursor: string }`.
  For `huge`, the totals are 10,000 and 40,000, `committed_from_sequence` is 38,401 and
  `next_cursor` is `scenario-cursor/1600`. Cursors use
  `scenario-cursor/<newest-relative-offset>`: the offset counts entries from the newest end,
  independently of how many entries are committed. Native readers see committed pages only;
  replay uses the seed and world metadata to generate requested older ranges deterministically.
- Size budget: one file at most 8 MiB, the whole tree at most 24 MiB. A unit test enforces both.
  The bounded committed history keeps `huge` within these unchanged budgets without dropping
  roster agents or introducing a different fixture encoding.

`scripts/emit.ts` is the only writer. `emit --check` generates into a temporary directory and
fails on any byte difference, naming each stale or missing file and the command that fixes it
(the genie-freshness pattern).

## Recordings (D3)

Synthetic data covers shape and scale; recordings supply the wire details a generator does not
invent (field combinations, revision chains, fences, page boundaries, real `TerminalScreen` runs).

```
recordings/<name>.script.json        fake-harness script generated from the world's cast
        │
        ▼
st3 scenario recorder (Rust test binary, run on demand)
  ├─ starts a scratch daemon in a fresh temporary state directory
  ├─ declares the cast's agents with a scripted fake harness
  ├─ the harness writes native transcript turns and executes commands in a real PTY
  └─ a client-v0 client sends a fixed list of subscribe commands and HTTP reads, and records each
     command, frame and HTTP exchange with its receive time
        │
        ▼
recordings/<name>.recording.jsonl    raw commands, frames and exchanges, scratch identities only
        │  scripts/emit.ts (import = normalization N, below)
        ▼
recordings/<name>.normalized.json    the fidelity oracle
        │
        ▼
slices of the recording's own world, and recorded slices reused by narrative worlds
```

- The recorder refuses to run against any daemon it did not start, and against any state
  directory it did not create. No live host data can enter.
- A script is generated from the world's cast, so the recorded agents are the cast members.
- Recording is not part of CI: it needs a built daemon. Committed recordings are inputs to
  `emit`; the freshness, decode, privacy and fidelity gates cover them. A re-recording that
  changes the wire shape is a reviewed diff.

First compile with `nix develop -c gate-slot --class gate -- cargo check -p st3 --tests
--locked`. Once clean, run the cheap recorder tests and all seven ignored
`-p st3 --test scenario_recorder` captures sequentially in one `gate-slot --class heavy`
lease inside `nix develop`, with `-- --ignored --nocapture` for captures. Set
`ST3_SCENARIO_SCRIPT` and `ST3_SCENARIO_OUTPUT` to absolute paths; the output must not
already exist. Re-record into a new candidate, then review it before replacing the committed
input. The recorder accepts no caller-owned endpoint or state directory. Failed scripts are
reported individually; a second heavy lease is restricted to failed scripts.
Fresh capture identities are inputs to the generated script's identity map; regeneration
preserves cast ownership and story content rather than hand-editing generated prompts.
Each command-bearing turn has its own projected-screen revision fence and a harness-emitted
per-turn completion marker after stdout. Earlier or intermediate screen changes cannot
satisfy the current turn's barrier. All recorder subprocesses, including curl and PTY
cleanup, use the scratch environment; curl disables configuration loading with first-argument
`-q`, and generated shell literals quote inherited paths as data.

The implementation composes three existing patterns, not a pre-existing recorder API:
`crates/st3/tests/terminal_binding.rs` supplies scratch ownership and teardown scaffolding;
`crates/st3/tests/no_st2_seat.rs` and `scripts/st3-no-st2-eval/{run,claude}` supply native
Claude transcripts and real driver hooks; `terminal_actions_survive_unrelated_writes_but_reject_changed_screens`
in `crates/st3-daemon/src/api/client_v0.rs` supplies the real PTY/projector and terminal attach pattern.
`terminal_binding` alone produces neither native transcripts nor styled screens.

### Recording formats

`scripts/recording-script.schema.json` is the authoritative field contract for both the
Rust recorder and TypeScript decoder (including required fields and unknown-field rejection).
Version 1 is the retained single-agent proof contract: `format_version: 1`, `world`,
selected cast `agent`, `host`, `person`, public `workspace`, explicit scratch-to-cast
`id_map`, ordered `turns` (`prompt`, `answer`, native `timestamp`, optional shell
`command`), and ordered HTTP GET `reads`. The seven generated narrative scripts use
version 2, with an explicit `agents` cast (`agent`, `host`), `historical_turns` prefix
count, `empty_baseline`, optional `walk_pages`, and `reconnect_after` turn indices. Turns identify their cast
`agent`; optional `tool` carries native tool-use `id`, `name`, JSON `input`, and JSON
`result`. Commands run only in the selected terminal owner's harness.
Historical turns complete before the baseline snapshot; native timestamps describe the
past without sleeps or receive-clock scaling. An empty baseline subscribes before creating
the seats. Reconnect steps disconnect the real client and resubscribe on a new socket,
capturing authoritative replacement frames, not fabricated sync events.
When `walk_pages` is enabled, the recorder follows each timeline read's actual
`next_cursor` until `has_more: false`, recording every HTTP route/query/status/body.
The hunt uses `limit=50` so its more-than-50-entry native history exercises older pages.
Recording privacy scanning decodes JSON/JSONL before checking capability field names and
values, including Unicode-escaped keys.
Both the scanner and recording decoder reject duplicate JSON object member names before
parsing discards earlier values, including names equivalent after Unicode-escape decoding.
Schema identities include `x-st-ref` annotations and `$ref` names ending in `Id`; neither
reference vocabulary may bypass the explicit identity map.

Version 1 raw JSONL rows contain `ordinal` (zero-based, strictly increasing), `socket_id`
(stable per connection, null outside sockets), `receive_ms` (monotonic elapsed milliseconds
from capture start), `kind`, and `value`. Kinds and values are:

- `header`: `format_version`, `script_format_version`, `world`, `daemon_build` (the
  checkout-built daemon's version/build provenance), `daemon_sha256` (binary digest),
  and wall-clock `started_at`;
- `command` / `frame`: the exact sent collection command / received JSON frame;
- `http`: `method`, `path`, `query`, `status`, `request_body`, and complete response `body`;
- `attach`: the submitted terminal attach action, also included in its HTTP exchange;
- `harness`: the script turn released to the harness;
- `pty_output`: the actual command stdout bytes represented as UTF-8 `text`, with `turn`
  and `terminal`; styled screen frames still come from the real projector;
- `baseline`: explicit `last_pre_timeline_ordinal`, identifying the last baseline frame.

Ordinal breaks ties between rows received in the same millisecond. The recorder replaces
attach capabilities with non-functional equality-preserving `capability#n` tokens **before
any file write**, including HTTP bodies and subscription commands. Scratch filesystem paths
are mapped to the script's public workspace before capture; raw committed files are already
public-safe, not an archive of credentials waiting for normalization.

The combined committed script, raw, and normalized recording inputs have a separate 12 MiB
budget; the fixtures-tree 24 MiB budget is unchanged.

The recorded-world end state is seven story-specific narratives: `fleet-mid-refactor`,
`ci-failing-fix`, `merge-conflict-standoff`, `long-debug-rabbit-hole`,
`release-candidate-polish`, `offline-reconnect-storm`, and `first-run-onboarding`.
This recording stage commits all seven scripts, raw captures and emitted normalized oracles.
The catalog's 146 fixtures remain byte-identical: adoption of recorded conversation/terminal
slices is the next stage, not an overlay or a change to world definitions here.

### Narrative capture selection

Each capture owns one scratch daemon on the narrative terminal owner's declared host.
Other-host agents remain synthetic; no identity map changes their host ownership. Selection
uses the committed cast, including future cast entries for initially empty worlds. The
recorder's terminal viewport is bounded to 80 columns by 24 rows.

| World | Recorded cast and story | Synthetic cast retained |
| --- | --- | --- |
| `fleet-mid-refactor` | Atlas builder and migrator on `host/meadow`: blocked worker typecheck and the fetchUser-to-loadUser migration, with real builder terminal output. | Reviewer and docs on `host/foundry`; their waiting review and idle-to-start milestones remain unchanged. |
| `ci-failing-fix` | Atlas builder on `host/meadow`: omitted-limit regression, bisect, default patch retaining the dependency bump, green retry. | Existing roster/work/card timing at 10/18/30 seconds remains synthetic. |
| `long-debug-rabbit-hole` | Atlas builder on `host/lantern`: three wrong hypotheses and forty minutes of native historical timestamps, terminal evidence, and the one-line fix. | Existing future milestones retain their synthetic timing. |
| `first-run-onboarding` | Sprout's future cast on `host/quarry`: subscription-before-create, empty conversation/terminal baseline, first native turn and green command. | First mission card and onboarding roster/details/attention remain synthetic. |
| `merge-conflict-standoff` | Harbor builder and reviewer on `host/lantern`: both branches, actual native tool call/result carrying the same file/base/ours/theirs conflict, person decision, combined policy and cache verification. | Mission/card and roster milestones remain synthetic. |
| `offline-reconnect-storm` | Harbor builder on `host/foundry`: retained native entry identities and three real disconnect/resubscribe replacement windows. | Reviewer on `host/orchard`; synthetic sync close/backoff schedule at 2/8/16 seconds remains unchanged. |
| `release-candidate-polish` | Meadow builder and docs on `host/quarry`: completed patch-version/changelog/matrix history and terminal command evidence; all scripted turns precede baseline, so no future agent activity is recorded. | Reviewer on `host/lantern`, and the completed synthetic mission/roster/card state. |

Current Claude native ingestion emits `revision: 1` and `final: true`, even when the
same native entry is rewritten. Scripts may supply `revisions` (intermediate assistant
texts) to capture real same-id content replacements, but the recorder preserves the
daemon's revision/finality fields exactly. Increasing streaming revision chains for
the migrator and reconnect story remain synthetic in the next cutover stage; these
recordings do not claim increasing native revision numbers.

The generated selection has 8/8/22/3/10/7/4 turns in the table's world order,
with 1/3/7/1/2/1/1 terminal commands respectively. Historical prefixes have
5/2/19/0/6/2/4 turns. The scripts derive prose, tool data and terminal output from the
existing world's cast and content; shell commands use `printf` to paint that fictional
project output in a real PTY. They do not run a nonexistent Atlas/Harbor/Sprout/Meadow
checkout or claim that the displayed project test commands actually executed. Green/red
output is story evidence, while PTY execution and screen projection are real.
Version 2's authored `anchor` maps historical native timestamps to the scratch capture's
wall clock; future turns use their actual native write time. Receipt offsets are never
scaled to the synthetic world's milestone offsets.

The initial hunt capture selected all 76 authored turns and 25 commands. The daemon
emitted 152 real replacement conversation frames and 14 delta frames, up to 200 entries
per frame; raw input alone reached 13,351,803 bytes. The hunt harness only appended native
rows (no content rewrites), and the recorder wrote the received frames unchanged.
Client-v0's follower reopens authoritative pages after cursor gaps, so historical
native insertion exhibits quadratic replacement-page growth; this is a product-wire
finding, not a reason to omit recorded frames or increase the 12 MiB budget.
The generated capture selection therefore retains the first and final shuffled batch of
each hypothesis, all three headings/rejections, more than 50 native entries, the full
forty-minute timestamp span, summary, one-line fix and green run. The repeated middle
batches remain in the unchanged synthetic catalog world, not in the smaller recording.

### Normalization

`N` turns a raw recording into its normalized form. It is deterministic and applies to commands,
frames and HTTP exchanges alike:

1. **Identities:** scratch ids map to the world's namespace through one id map, built from the
   script's explicit `id_map`, including deterministic generated identities and path identities.
   An unregistered schema-located identity fails at its exact pointer. Opaque native payloads
   are not recursively rewritten. One scratch daemon represents one cast
   host: all captured host-bearing data maps to the narrative terminal's owning cast host;
   other hosts remain synthetic.
2. **Time:** receive times become offsets from the explicitly marked last pre-timeline frame,
   rounded to milliseconds, never scaled. Historical past comes from native transcript
   timestamps. Schema instants are rebased to the anchor as in [Time](#time-d5).
   The header's wall-clock origin is paired with the monotonic receive clock; the baseline
   witness is ordinal 7 at receive 30543 ms in the proof capture. Offset zero refers to that
   frame, not the later baseline marker. Replay starts its clock before the earliest input,
   so pre-baseline HTTP reads, attach and subscription commands execute at negative offsets.
3. **Fences and cursors:** each distinct raw `store_index` becomes an ordinal store version
   (`store#1`, `store#2`, …) in increasing order, and a snapshot id becomes the token of its store
   version and host; page cursors become `cursor#n`. Equal raw values get equal tokens, so two
   subscriptions read at the same store index keep the same token. Opaque transport request,
   operation, attachment, and capability ids become `request#n`, `operation#n`,
   `attachment#n`, and `capability#n`, preserving equality (capabilities are already scrubbed
   at capture).
   An action result can reference a later committed snapshot than its HTTP envelope.
   If no full snapshot object witnesses that typed `snapshot_id`, its daemon encoding
   `snapshot/<host>/<store>/<digest>` supplies only host/store identity for ordinal
   ranking. The host must match the cast; no timestamp or snapshot object is invented,
   and the distinct result/envelope fence relationship remains intact.
4. Nothing else changes: payloads, order, `has_more`, revisions and frame boundaries stay as
   recorded.

### Recording fidelity

Each recording has a **fidelity world**, `recorded-<name>`: every slice comes from the recording,
none from factories. Fidelity worlds are test-only; they are not in the catalog or in
`fixtures/scenarios`.

This stage provides capture-derived test-only `recorded-<name>` imports for all seven
narratives and checks each against its emitted normalization oracle. The existing
proof retains independent per-frame/per-HTTP replay fidelity and mutation tests.
Full replay fidelity for the new historical/reconnect captures is deferred to the
cutover stage: prebaseline historical replacement windows and connection lifecycles
need additional replay boundary representation. Import-oracle equality is not
presented as proof that replay already reproduces those windows.

The TypeScript proof world is `recorded-fleet-mid-refactor-proof`, defined only in the test tree.
Its six slices are imported from captured state and events. Optional typed `recording` transport
metadata preserves frame ordinals, receive offsets, selectors, event indices (or baseline),
replacement/`has_more` flags and fences; HTTP metadata preserves method, route, query, status,
envelope ids, page boundaries and attach fields. Frame metadata never contains response items.
HTTP timeline boundaries contain only entry-id selections and block-field presence: the captured
HTTP page omitted native blocks that the socket carried for those same entries. Replay selects
the entries from folded slice state and applies that projection without changing their other
fields. Replay reconstructs item arrays and terminal values from the slices, not from the
oracle's response table. The normalized oracle is a separate emitted recording input.


The fidelity test replays `recorded-<name>` with `now` pinned to the anchor, sends the normalized
commands and HTTP reads at their recorded offsets, normalizes the replay output with the same
`N`, and compares with the normalized recording:

- per subscription: the sequence of frames, each with its kind, boundaries (which items one frame
  carries), items, revisions, `order`, `has_more` and store-version token;
- per HTTP exchange (including attach): method, route, query, status and full body, with cursor tokens.

Fences are compared as tokens, so the oracle checks the equality relationships between fences
(two frames at the same recorded store version share a replayed store version, and frames at
different versions do not) without equal raw values. Fidelity worlds take each event's `store`
from the recording; see [Store versions](#store-versions).
The same `N` applies to replay outputs in normalized context: numeric slice stores become
oracle `store#n` tokens without reranking; existing normalized tokens remain unchanged.
Independent mutations to slice items/revisions and typed metadata `has_more`, frame boundaries,
store fences and HTTP status must fail fidelity while leaving the oracle untouched.

The required narrative end state reuses a recording's `conversation` and `terminal` slices and
generates other slices from the same cast. This recording stage does not perform that cutover:
the ordinary 24 catalog worlds and their fixture bytes remain unchanged. Edge-state worlds
remain synthetic. The proof capture has a reusable attachment; it does not prove a single-use
capability recording or complete native PTY-server asciicast. Narrative PTY output records
the actual scripted command stdout, not an invented full native-project scrollback.

## Replay (D8)

`createReplay(world, { clock })` returns a transport for the real data layer:

```ts
const replay = createReplay(loadWorld('fleet-mid-refactor'), { clock: realClock() })
replay.socket   // CollectionSocketFactory (also accepted where a TerminalSocketFactory is)
replay.fetch    // typeof fetch: discovery, list and detail reads, older pages, actions
replay.actions  // actions the client submitted, in order, for test assertions
```

- **Seams:** the generated client already accepts `fetchImpl` and `socket`; the fractal-web SDK
  passes them through `St3Options.fetch` and `St3Options.socket`. Replay adds no other hook.
  The socket accepts and ignores command keys it does not use, such as the `trace` field the
  web client adds to a subscribe command.
- **Windows:** the first reply to a collection subscription is a `snapshot` with the slice state
  filtered by the subscription's filters, in display order, cut to its `limit` (1–200), at the
  current store version.
- **HTTP:** `fetch` answers `GET` reads from the same state, paging by cursor within the advertised
  bounds. `terminal.attach` returns the terminal's current incarnation and a capability that the
  socket accepts. Other actions are recorded in `replay.actions` and answered with an accepted
  `ActionResult` and an operation that completes; a world may declare a different answer per
  action. Replay does not simulate the effects of actions.
- **Seeded history:** conversation pages first serve committed entries, then generate only the
  requested older range from `ConversationThread.history`, using its seed and world at
  `world.now`. Each turn is independently seeded; generation uses neither `Math.random` nor
  `Date.now`. Timeline cursors remain `scenario-cursor/<newest-relative-offset>` across the
  committed/generated boundary. The `huge` scripted consumer starts with the newest 50 entries
  and requests older pages, including beyond the committed 1,600-entry window. Native file
  readers have only the committed pages; the full logical history is available through replay.
- **Clocks:** `realClock()` schedules with timers; `manualClock()` exposes `advance(ms)` for unit
  tests. Replay never waits on its own; it runs only what the clock releases.

### Store versions

The replay models one store per world, as st's SQLite store index is one counter per host. The
initial synthetic state is store version 1. Each timeline event that changes state names its
resulting version in numeric `store`; synthetic worlds increment it by one per state-changing
event. Fidelity worlds preserve imported versions unchanged, so several recorded events can share
one version. Their baseline is not necessarily version 1: older HTTP fences may precede it.
Events that change no state (sync transport events) keep the current version.

- Every collection `snapshot`/`changes` frame, every terminal `screen` frame, and every page
  response carries a fence for the store version current when it is built; only conversation
  frames carry none. `store_index` is the version and the snapshot id is derived from host and
  version. A screen does not advance the store.
- Subscriptions read without an intervening state change carry the same fence.
- On each socket, `store_index` is nondecreasing; it increases only when the store advances.
- Captured HTTP fences retain their published version even when it lags a newer socket fence;
  fidelity does not impose global receive-order monotonicity across HTTP and socket streams.

### Replay dispatch

Every timeline event tag has exactly one transport behavior. The dispatch function is exhaustive
over the event union (a `never` check), and each row has a test through the real client handler.

| Slice | Event | Transport behavior |
| --- | --- | --- |
| roster, details, attention | `changes` | Updates the state. For every open subscription whose filtered, limited window changes: one `changes` frame with `upserts`, `removes` (including rows leaving the window), the complete new `order`, `has_more` and a new fence. Unaffected subscriptions get nothing. |
| conversation | `entries` | One `conversation` frame with `replace: false` and those entries, without `has_more`, to subscribers of that agent or session. |
| conversation | `replace` | One `conversation` frame with `replace: true`, the newest page (`page_size`) and `has_more`. |
| conversation | `thread-create {thread}` / `thread-remove {agent}` | Introduces / removes full thread membership. Creating an existing agent sends ready subscribers a replacement frame and retains their agent binding across session changes; pending first replies obey hold/loading policies. Removal sends `not-found` to every matching subscriber and later reads. |
| terminal | `terminal-create {record}` / `terminal-remove {terminal}` | Introduces / removes full terminal membership. Runtime reads, attaches and subscriptions see new metadata. Existing ready subscriptions get the replacement screen for an unchanged incarnation, `stale-fence` for a changed incarnation, or `terminal-ended` for an exited runtime. Removal sends `not-found` even to held/loading subscriptions, and later reads/attaches. |
| terminal | `screen` | One `screen` frame with the `TerminalScreen` to subscribers holding the current incarnation. |
| terminal | `unavailable` | An `error` frame, code `terminal-unavailable`, `retryable: true`. A fresh `terminal.attach` returns the current incarnation and a capability, and a subscribe with it resumes with the current screen. A subscribe that reuses the earlier lease also resumes while the lease is valid (a separate protocol test; the SDK always attaches afresh). |
| terminal | `end` | An `error` frame, code `terminal-ended`, `retryable: false`, to every subscriber. Later attaches and subscribes return `terminal-ended`. |
| terminal | `incarnation` | Sets a new incarnation. Current subscribers get an `error` frame, code `stale-fence`. A subscribe with the old incarnation gets `stale-fence`; `terminal.attach` returns the new incarnation and a new capability. |
| sync | `open-fail {opens?}` / `open-ok` | Each failed open emits `onerror`, then `onclose` code 1006 and no body. Omitted `opens` fails one open; a positive integer fails that many opens; `'all'` fails every open until `open-ok` or a later `open-fail` replaces the policy. |
| sync | `open-hold` / `open-release` | Subsequent socket opens stay pending without `onopen`, `onerror` or `onclose` until release. Release schedules their normal opening callbacks at the current clock instant; already-open sockets are unaffected, and closed pending sockets never reopen. Other open-failure policies remain independent. |
| sync | `http-raw {route, when?, status, content_type, body}` | Matching requests return a non-client-v0 response (for example a proxy's HTML page). The decode gate skips `body`; the privacy gate still scans it. |
| sync | `close {code, reason}` | Closes the open socket with that WebSocket close code. |
| sync | `reopen {after_ms}` | Allows a new socket open again after `after_ms`; until then opens fail with close code 1006. |
| sync | `http-error {route, when?, status, envelope}` / `http-ok {route, when?}` | Matching requests return the fault until cleared. `when.cursor` is `'present'` or `'absent'`; every `when.query` string must equal the URL query value. An older-page-only fault therefore leaves the first page healthy. Exact pathname wins over route family, then generic `resources`; newest matching policy wins within a route. An unqualified `http-ok` clears all policies for that route; a qualified one clears only its matching condition. |
| sync | `hold {selector}` / `release {selector}` | Withholds / then sends the selected subscription's first reply. |
| sync | `resync {selector, code?, message?}` | A `resync` frame with `retryable: true`; `code` and `message` only when given (uncoded resync is the legacy shape). |
| sync | `error {selector?, code?, message, retryable, repeat?}` / `error-clear {selector?}` | Sends the error immediately. `repeat: true` also rejects every later matching subscribe until cleared; repeated rejection removes the attempted subscription. Without selector the frame has no id or collection, and without code it remains uncoded (the legacy subscription-cap shape). Selector-qualified policies affect only that surface. |
| sync | `notice {peers}` / `notice-clear` | Sets / clears the `SyncNotice` that every subsequent page response (`Page`, `ArrangementPage`, `ResourcesPage`) carries in `sync`. No socket frame. |
| sync | `progress {selector, stage, …}` | A `progress` frame. Active only when the world's capabilities advertise `sync-status.v1` and the frame is in the client-v0 schema. |
| sync | `heartbeat` / `silence {ms}` | A `heartbeat` frame / no frame of any kind for `ms`. Same activation as `progress`. |

Events marked "same activation" are rejected by the emitter until the client-v0 schema defines
those frames, so no scenario sends a frame the strict decode gate cannot check.

### Consumer folds

Each stream is checked with the consumer that really folds it, at every clock step of every
world and variant, against the expected state the slice defines:

| Stream | Real consumer | Expected |
| --- | --- | --- |
| collection windows | the client's `applyWindow`, per subscription | slice state filtered, ordered and cut to that subscription's limit |
| conversations | the conversation reducer in `@smalltalk/st3-views` (`sessionView`) | the slice's entries for that agent, joined as the reducer documents |
| terminals | the SDK terminal follow in `@st3/sdk/effect` (`followTerminal`: attach, `subscribeTerminal`, screen decoding, and the shared follow's error and retry handling) | the latest screen at each step; after `terminal-unavailable` and after `stale-fence`, a fresh attach (runtime read plus `terminal.attach`) and a resubscribe that continues with the current incarnation's screen; `terminal-ended` ends the follow |
| sync | the SDK sync fold in `@st3/sdk/effect` | the [sync matrix](#sync-matrix) `expected` status per surface |

Windows and conversations run in this package's tests. The terminal and sync checks run in the SDK
package, which takes the kit as a development dependency (see
[Consumer contracts](#consumer-contracts-d2-d8)). Separately, this package tests wire delivery for
every terminal event: the generated `collectionStream` receives the expected `screen` and `error`
frames in order, and each frame decodes as a `CollectionFrame` in strict mode, including the
screen frame's `snapshot`. `huge` checks protocol-sized windows and scripted older conversation
pages, including deterministic generation beyond committed history, not a full 1,000-agent
socket window. The normal roster slice still commits all 1,000 agents.

## Sync matrix

The `sync` slice's variants are rows of this matrix. Each row names a trigger (a wire script the
replay dispatches, or a local condition the consumer test creates) and the `SyncStatus` the SDK
must report. `SyncStatus` is the portable Amendment v2 contract of `@st3/sdk/effect`
(`sync-status.ts`). Times in `expected` are offsets from `now`. "Pending" rows wait for server
frames (PR A: heartbeat, coded reasons, `subscription-limit` code; PR B: progress) to enter the
client-v0 schema; until then they are excluded from committed fixtures and the gates.

| Variant | Trigger | Expected `SyncStatus` |
| --- | --- | --- |
| `live` | snapshot / first conversation frame | `Live{since}` |
| `connecting` | socket open held | `Connecting{attempt: 1}` |
| `requested` | open; `hold` on the surface's subscription | `Requested{since}` (view tier derives Stalled after 5 s) |
| `socket-dropped` | after Live: `close {1006}`, opens keep failing | `Stale{Reconnecting{attempt n, nextAt, issue}, lastLiveAt}` |
| `reconnected` | `socket-dropped`, then `reopen` | `Stale{Reconnecting}`, then `Live` from the new snapshot |
| `closed-unknown` | close without a scheduled retry | `Stale{Unknown}` |
| `resync-coded` | `resync {code: remote-unavailable, message}` over a live conversation, or the live agents window when the world has no initial conversation | `Stale{Resync{code, message, attempt}}`, content kept |
| `resync-uncoded` | `resync` without code | `Stale{Unknown}` |
| `forbidden` | capabilities `http-error`: 403 with an `ErrorEnvelope`, code `forbidden` | `Failed{Server{forbidden, message}}`; reconnecting stops |
| `non-client-response` | capabilities `http-raw`: 403 with an HTML body | the probe failure is not a client-v0 rejection, so the SDK keeps retrying: `Stale{Reconnecting}` (or `Stale{Unknown}` without a scheduled retry) |
| `open-fail` | capabilities succeed; `open-fail` on every socket open | `Stale{Reconnecting{attempt n, nextAt, issue}}` |
| `subscription-error` | `error {selector, code: not-found, retryable: false}` | `Failed{Server{not-found}}` on that surface only; others stay `Live` |
| `subscription-limit-local` | local: capabilities without `collections` v1 (cap 8), the test opens 9 visible follows | `Failed{Local{subscription-limit, cap: 8}}` |
| `subscription-limit-legacy` | `error` without code or collection until the lowered cap is full | `Failed{Unknown}` |
| `evicted` | local: a hidden follow loses its slot to a visible one | `Stale{Evicted}` (never shown) |
| `capability-absent` | capabilities omit the surface's capability (e.g. `terminal.attach`) | the surface's unsupported failure as the web source reports it today |
| `rate-limited` | `http-error {route, 429, rate-limited}` on a list read | `Failed{Server{rate-limited}}` for that HTTP-only surface |
| `cursor-gap` | `http-error` with code `cursor-gap` on the events read | `Failed{Server{cursor-gap, message}}` for that read; live windows unaffected |
| `page-cursor-expired` | `http-error` with code `page-cursor-expired` on an older-page read | `Failed{Server{page-cursor-expired, message}}` for that read; live windows unaffected |
| `subscription-limit-coded` (pending PR A) | `error {code: subscription-limit}` | `Failed{Server{subscription-limit}}` |
| `quiet` (pending PR A) | Live, then `silence {15000}` | `Stale{Quiet{lastFrameAt}}`; at 25 s close and reconnect |
| `progress` (pending PR B) | `progress` queued → resolving → routing{host} → reading{done,total} | `Progress{stage, elapsedMs, stageSince, reportedAt, host?, done?, total?}` per stage |
| `progress-absent` | no `sync-status.v1` capability, slow first snapshot | `Requested` → `Live`; no invented stage; quiet detection off |

In every `Failed{Server}` expectation, `code` and `message` equal the triggering envelope's
`code` and `message`. The SDK never replaces a server code.

Each expectation is `{ surface, at_ms, status, compare }`. `surface` is a collection (`agents`), a
selector-qualified stream (`conversation:<agent>`, `terminal:<terminal>`), an HTTP-only read
(`read:<route>`) or a local follow (`follow:<n>`). `compare: 'exact'` compares the whole status;
`compare: 'shape'` compares tags, server codes and messages, and local kinds and caps, and ignores
attempts, instants and issue texts the SDK chooses.

Where an `expected` value depends on SDK details this document does not fix (the
`capability-absent` failure, `attempt` numbering), the row starts as `shape`; the SDK's current
behavior is recorded in the row on first adoption (`exact`) and changes only with the SDK contract.

Capability advertisement is discovery, not an independent server admission rule. The client-v0
server constructs registered capabilities with granted/ungranted/unavailable states
(`crates/st3-daemon/src/api/client_v0.rs`, `capabilities`), checks action scopes in `action`, and opens
terminal subscriptions through `open_terminal_subscription` and the attachment lease checks.
It has no unsupported failure triggered solely by omission from that advertisement.
Replay therefore imposes no new attach or subscribe restriction when an entry is omitted.
The `capability-absent` SDK row remains `compare: 'shape'`; its consumer-side unsupported
failure is pinned on first web adoption, not invented by the transport.

**Replication divergence is not a sync failure.** A `SyncNotice` with state `diverged` says the
host projects a different graph from the same envelopes; the transport is healthy. It has its own
edge world, `replication-diverged`, and the header shows it as the UI contract describes.

## React binding (D8)

```tsx
<ScenarioProvider world={world}>
  <Sidebar />
</ScenarioProvider>

const rows = useScenarioSlice('roster', toSidebarRows)
```

The kit hands out **wire state**, not decoded values:

- `useScenarioSlice(slice)` returns `WireSlice<slice>`: the slice state at the current replay
  time, rebased, typed by `Models.generated.ts` (timestamps are strings, nullable fields are
  `null`, unknown enum values stay strings). For `sync` it returns the plain portable
  `SyncStatus` per surface from the matrix, which needs no decoding.
- `useScenarioSlice(slice, project)` returns `project(wireSlice)`. A projection turns wire state
  into a component's props (its DTO). Projections and DTOs belong to the consumer, because
  component props are consumer-owned.
- A consumer whose app decodes with the Effect codecs (`@smalltalk/st3-client/schema`) or folds
  with the SDK uses those same functions inside its projection. The consumer may import Effect;
  the kit does not. So a story and the app derive props through one source of truth.
- **Conformance:** for every world, the consumer tests that its story projection and its app
  projection (through replay and the real data layer) produce equal props for each component
  that has both.
- The provider owns a manual or real clock, so a story can advance time (streaming text, a
  reconnect) without timers in components.
- Components under the provider never import the kit's factories; they receive props.

## Storybook binding (D10)

```ts
// .storybook/preview.ts
import { scenarioGlobalTypes, withScenario } from '@smalltalk/st3-scenarios/storybook'
export default { globalTypes: scenarioGlobalTypes, decorators: [withScenario] }

// Sidebar.stories.tsx
export default {
  component: Sidebar,
  parameters: { scenario: { slices: ['roster', 'attention'] } },
  argTypes: scenarioArgTypes(['roster', 'attention']),
}
export const Huge = { parameters: { scenario: { world: 'huge' } } } // pinned
```

- `scenarioGlobalTypes` adds the `scenario` toolbar item (the catalog, default
  `fleet-mid-refactor`) and `scenarioNow` (pinned time; default: load time).
- Each declared slice becomes an arg of the same name; its control selects a variant. Default
  arg value `default` means "the world's own slice".
- Precedence: a story's pinned `parameters.scenario.world` wins over the toolbar; an arg other than
  `default` replaces that slice through `world.with`.
- Deep links use Storybook's own URL state: `?globals=scenario:ci-failing-fix&args=conversation:long`.
- `withScenario` wraps the story in `ScenarioProvider` and renders `data-scenario="<world>"` and
  `data-scenario-slices="<slice>:<variant>,…"` on its wrapper.
- Explorer pickers (theme, density, layout variants) are separate globals or args. A scenario
  selection never changes rendering options, and the reverse.
- App-level stories use `createReplay` instead of `useScenarioSlice` and render through the real
  data layer.

### Consumption check

An annotation alone proves nothing, so the kit tracks reads. The provider records each slice a
story reads through `useScenarioSlice`, and each slice whose collections an app-level story's
replay serves. `scenarioStoryCheck(story)` renders a story through Storybook's portable stories
and fails unless all hold:

1. **Reads:** every declared slice was read; no undeclared slice was read. This applies to every
   story.
2. **Data dependence:** each slice has **contrast pairs**: two variants over the same cast whose
   wire state differs (for example `roster` `default` and `one-agent`). Shared cast members may
   appear on both sides. For each side the pair defines what the render must show:
   - **expected markers:** display names or short texts from that side's data that must appear;
   - **distinguishing markers:** texts that must appear on this side and not on the other (for
     example the names of agents `one-agent` removes);
   - **counts** where a subset contrast has no unique text: the number of rendered rows of a
     declared kind (for example agent rows: 4 versus 1);
   - **states** for sides without content (`empty`, `loading`, `none`): the side shows none of
     the other side's distinguishing markers and the renders differ.
   A pair passes when every side meets its own expectation.
3. **World switch:** for stories that do not pin a world, the check picks one representative pair
   of catalog worlds per non-invariant declared slice whose wire states and visible contrast
   expectations differ. Worlds sharing the slice, such as `failed-sync-*` worlds for a roster
   story, are never paired. Marker multisets and contentless states define content-slice contrasts;
   portable statuses define sync contrasts. Transport scripts or replication metadata alone do
   not require a roster-name story to render differently. The pair applies check 2's rule.
4. **Pinned world:** for stories that pin a world, changing the toolbar global leaves the render
   unchanged; check 3 does not apply.
5. **URL state:** rendering from the URL state (`globals` and `args`) equals rendering from the
   same values set directly.

A story may mark a slice as intentionally invariant in its rendering
(`parameters.scenario.invariant: { sync: 'reason' }`), for example a story that reads `sync`
only to choose a layout. Checks 2 and 3 skip that slice; check 1 still requires the read. A
reason is required.

The kit's own tests include a negative case: a story that declares slices but renders fixed text
fails checks 1 and 2.

## Catalog (D9)

Narratives:

| World | Story the data tells |
| --- | --- |
| `fleet-mid-refactor` | Four agents rename an API across a monorepo. One is blocked on a typecheck, one waits for review, one streams diffs, one is idle. The default world. |
| `ci-failing-fix` | `main` is red after a dependency bump. An agent bisects, writes a minimal patch, and the retry turns green. |
| `long-debug-rabbit-hole` | A forty-minute hunt for a flaky test: three wrong hypotheses, long terminal scrollback, a one-line fix. |
| `first-run-onboarding` | A fresh daemon: no agents, then the first agent, the first mission launch card, the first green run, over the timeline. |
| `merge-conflict-standoff` | Two agents edit the same files. A person adjudicates through review cards with a three-way diff. |
| `offline-reconnect-storm` | The socket drops mid-stream repeatedly. Stale badges, backoff, resubscription and authoritative snapshots. |
| `release-candidate-polish` | A calm tail: changelog, version bump, a green matrix, mostly idle agents, few cards. |

Edge states:

| World | Content |
| --- | --- |
| `empty` | Live connection, zero resources in every collection. |
| `loading` | Discovery succeeds; every first reply is held. |
| `unknown-fields` | Future enum values, extra keys, unknown resource and entry kinds. Decoded tolerant; strict decoding fails exactly at the declared paths. |
| `failed-sync-<cause>` | One world per failed or stale [sync matrix](#sync-matrix) row that is not pending and needs no local condition: `socket-dropped`, `open-fail`, `resync-coded`, `forbidden`, `non-client-response`, `subscription-error`, `subscription-limit-legacy`, `capability-absent`, `rate-limited`, `cursor-gap`, `page-cursor-expired`. Each uses `fleet-mid-refactor`'s cast and other slices. Pending rows become worlds when their frames land. |
| `replication-diverged` | Healthy transport; page responses carry a `diverged` sync notice for one peer. |
| `huge` | Full 1,000-agent roster across many hosts and missions in the normal slice; the first cast agent has 10,000 logical turns (40,000 entries). Commits the newest 400 turns (1,600 entries), including a 50-entry live window; scripted replay pages reach older entries generated deterministically from seed metadata. Native readers see committed pages only. |
| `unicode` | RTL, CJK, emoji, combining marks, zero-width joiners, 200-character names and long paths in every slice. |

`evicted`, `quiet` and `progress` stay variants: they need a local condition or a pending frame.

## Gates (D4, D6)

The path-filtered `fractal-web-execution` job runs these gates as lanes of `scripts/ci-fractal-web`
(`typecheck`, `test`, `emit:check`, `decode`, `scan`). Its change detection includes
`clients/typescript/st3-scenarios`, `fixtures/scenarios`, `docs/st3/client-v0`,
`clients/typescript/st3-client` and `clients/typescript/st3-views`:

1. Typecheck and unit/property tests: determinism (same seed, same bytes); rebase (the shared
   vectors, and for any `now` every listed instant stays valid with differences exact relative
   to the millisecond-truncated inputs, and no unlisted value changes); replay dispatch (one test
   per event row); store versions (nondecreasing per socket, equal without a state change);
   consumer folds for windows and conversations; terminal wire delivery; recording fidelity;
   consumption check, including its negative
   case; size budget; no `Math.random`/`Date.now` in generators; no Effect import in runtime
   exports.
2. Freshness: `emit --check`, including each file's `times` list and the normalized recording
   regenerated from committed script/raw inputs. Only `emit` writes normalized artifacts.
3. Decode: every wire value in every committed slice and in every in-process variant is decoded
   with `@smalltalk/st3-client/schema` in strict mode, once at the anchor and once rebased to a
   random `now`. `huge` is the exception: every variant decodes at the anchor, and only its default
   slices decode at the random `now`, which runs the rebase path on scale-sized data and keeps the
   lane bounded. `unknown-fields` decodes tolerant and fails strict at exactly its declared paths.
   Recording scripts, raw NDJSON and normalized JSON are decoded independently; imports
   strict-decode all capture-derived slice values at anchor and rebased time. Corrupt or stale
   inputs fail this gate. Combined script/raw/normalized recording bytes are limited to 12 MiB
   independently of the unchanged scenario fixture budgets.
4. Privacy: `scripts/check-public-repo` covers the committed files and fails the gate when it
   cannot run, is killed, or exits non-zero without a finding in this package or the scenarios.
   `scripts/scan.ts` walks every published file of the package (skipping `node_modules` and
   gitignored output) and the committed scenarios with the token, credential, internal machine
   name, real user name and home-path rules of the earlier synthetic-fixtures scan (the home-path
   rule is case-sensitive, so a source path such as `src/users/` passes). It also rejects email
   addresses outside the reserved example domains, IPv4 addresses outside the documentation ranges
   and loopback, and URLs whose host is not `scenario.invalid`, `localhost` or an `example.com`,
   `example.org` or `example.net` name. It checks that every subject reference is in the
   [identity namespace](#identity-namespace), and rejects date and clock patterns in vocabulary
   banks.
   Recording inputs additionally reject scratch paths and live terminal capabilities; only
   numbered nonfunctional `capability#n` placeholders are allowed. Registered scratch identities
   in raw captures are validated by the script-directed normalization gate, not treated as
   already-normalized scenario identities.
5. Consumers: the SDK sync-matrix fold, stui's contract test, the Rust and Swift rebase vectors
   and the fractal-web bundle check run in their own jobs (see below).

Every gate has a test that plants one violation (a strict-invalid value, a stale file, a
denylisted host, a story that ignores its data, a wrong sync expectation) and shows that the gate
fails. Any change to the client-v0 schema re-runs the decode gate, so a contract change that
breaks a scenario fails in the same pull request.

## Consumer contracts (D2, D8)

| Consumer | Owner | Contract |
| --- | --- | --- |
| fractal-ui Storybook | fractal-ui maintainers | `preview.ts` installs `scenarioGlobalTypes` and `withScenario`. Every story declares `parameters.scenario` and passes `scenarioStoryCheck`. Projections from wire state to component props live in fractal-ui and use the shared codec/SDK functions where the app does. Hand-typed fixtures (`Workshop` data, `embrace-fixtures`, `sync-fixtures`) are replaced by slices. |
| `@st3/sdk/effect` | fractal-web maintainers | Takes the kit as a development dependency. A test runs every non-pending sync matrix row through the real SDK fold and compares with `expected`; a type-level assertion keeps the kit's portable `SyncStatus` equal to `sync-status.ts`. A second test runs every terminal variant through `followTerminal` and checks screens, the fresh attach and resubscribe after `terminal-unavailable` and after `stale-fence`, and `terminal-ended`. |
| fractal-web dev, tests, e2e | fractal-web maintainers | A dedicated Vite mode builds the app from a scenario entry: its own HTML page without the early roster connect script of `src/web/index.html`, and a runtime built with `adoptEarlyCollections: false` that passes `replay.socket` and `replay.fetch` to `St3Options`. No socket reaches a real gateway in this mode. Production code never reads a URL parameter or runtime flag to select fake data. A bundle check, modeled on `src/telemetry/measurement/check-bundles.mjs`, fails when the production bundle contains scenario-kit code. e2e runs on replay; story and app projections pass the conformance test; the earlier `scripts/synthetic-fixtures` generator is retired. |
| stui | native-client maintainers | `fixtures/clients/demo-world.json` is derived from `fleet-mid-refactor` through stui's own adapter (`ui/adapt.rs`); `STUI_UPDATE_CONTRACT=1` regenerates it, and the contract test fails on drift. |
| iOS app | native-client maintainers | Demo mode reads the same generated data: the derived `demo-world.json`, or `createReplay` (the app is TypeScript) through its gateway client seams. |
| Rust `st3-client` | native-client maintainers | Provides the rebase reader and passes the shared vectors. |
| Swift `St3Client` | native-client maintainers | Provides the rebase reader, passes the shared vectors, and decodes every committed scenario file with the Swift models. |
| fractal TUI | fractal-ui maintainers | Reads `fixtures/scenarios/<world>/<slice>.json` from a pinned smalltalk revision through its Rust adapter and the Rust rebase reader; its snapshot tests derive from those files. |

## Non-goals

- Simulating the effects of actions; replay records them and returns declared answers.
- Load or performance testing; `huge` checks rendering at scale only.
- A terminal emulator in the kit; full-screen terminal content comes from recordings.
- Absolute timestamps in authored data, unseeded randomness, network fixtures, and any real
  session, transcript or host capture.

## Acceptance

- Every fractal-ui story renders a scenario world, passes `scenarioStoryCheck`, and the Scenario
  toolbar switches it.
- fractal-web e2e runs on replay; the production bundle contains no scenario-kit code.
- The SDK fold matches every non-pending sync matrix row.
- The stui demo shows the generated `fleet-mid-refactor` world.
- fractal TUI and iOS show the same generated data; Rust and Swift pass the rebase vectors.
- CI decode, freshness, privacy, fidelity and consumption gates are green, and each fails on a
  planted violation.

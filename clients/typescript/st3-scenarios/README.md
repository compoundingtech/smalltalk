# @smalltalk/st3-scenarios

Seeded, privacy-safe scenario worlds for every Smalltalk client, and the kit that builds and
replays them. [spec.md](spec.md) is the contract.

The registered catalog contains 24 worlds: seven narratives, empty/loading/forward-compatibility
edges, eleven failed-sync causes, replication divergence, `huge` and Unicode.

```ts
import { loadWorld } from '@smalltalk/st3-scenarios'
import { createReplay, realClock } from '@smalltalk/st3-scenarios/replay'

const world = loadWorld('fleet-mid-refactor') // instants relative to the load time
const oneAgent = world.with({ roster: 'one-agent' }) // same cast, one slice replaced

// Real data layer over a fake gateway: pass both seams to the client or SDK.
const replay = createReplay(world, { clock: realClock({ start: world.now }) })
// St3Options: { socket: replay.socket, fetch: replay.fetch }
```

Components read wire state through `ScenarioProvider` and `useScenarioSlice(kind, project?)` from
`@smalltalk/st3-scenarios/react`. The provider owns a clock (a `manualClock` at `world.now` unless
one is passed) and records each slice a story reads. Projections from wire state to props belong
to the consumer.

```ts
// .storybook/preview.ts
import { scenarioGlobalTypes, withScenario } from '@smalltalk/st3-scenarios/storybook'
export default { globalTypes: scenarioGlobalTypes, decorators: [withScenario] }

// a story file
parameters: { scenario: { slices: ['roster'] } }, argTypes: scenarioArgTypes(['roster'])

// a test: checks reads, data dependence, world switch, pinned world and URL state
scenarioStoryCheck(Story, meta, { assert: true })
```

Rust, Swift and other readers use the committed files in `fixtures/scenarios/` and shift the
instants each file lists in `times` (vectors: `fixtures/scenarios/_vectors/rebase.json`).

## Huge world and history pages

`loadWorld('huge')` contains all 1,000 agents across many hosts and missions in the normal roster
slice. The first cast agent has a logical 10,000-turn conversation, with four entries per turn
(40,000 entries). The committed conversation contains only the newest 400 turns (1,600 entries,
sequences 38,401–40,000), including the 50-entry live window. The thread advertises
`page_size: 50` and `has_more: true`; a scripted consumer requests older pages rather than
receiving the whole history in its live subscription.

Optional `ConversationThread.history` records `{ kind: 'seeded-turns', seed: number,
world: string, total_turns: number, total_entries: number, committed_from_sequence: number,
next_cursor: string }`. In `huge`, the totals are 10,000 turns and 40,000 entries,
`committed_from_sequence` is 38,401, and `next_cursor` is `scenario-cursor/1600`.
Timeline cursors use `scenario-cursor/<newest-relative-offset>`, counting from the newest end
through both committed and generated history. Replay serves committed entries first, then
deterministically generates requested older ranges from the history seed and world at `world.now`;
each turn is independently seeded, without `Math.random` or `Date.now`. Native readers see only
the committed pages, not the full logical history.

Fixtures retain `fixtures/scenarios/<world>/<slice>.json`, one file per slice with no per-world
manifest or shards. The bounded history window preserves the unchanged 8 MiB/file and
24 MiB/tree budgets without trimming the roster.

## Authoring contracts

- Casts use either `roles` or `agents: { key, role, name?, workspace?, branch? }[]`.
  Stable unique keys determine identities and seeded forks; semantic roles may repeat. Workspace
  overrides stay under `~/src/<project>/`. `agent` also supports `state: 'desired'` without a runtime.
- `thread-create {thread}` and `terminal-create {record}` introduce complete resources after
  offset zero; `thread-remove {agent}` and `terminal-remove {terminal}` remove membership.
  Replacement creation refreshes ready conversation/terminal subscribers or rejects a stale
  terminal incarnation. Removal also refuses subscriptions whose first reply is still held.
  Use the shared `terminalRecord(ctx, member, run, startedMs)` factory, not a world-local helper.
- Future resource kinds live in the roster/details/attention state's optional `resources`
  array and in `changes.upserts`. Fold keeps them outside known-kind dispatch; replay sends
  the original encoded values over primary collection sockets/HTTP pages and generic resources
  HTTP. Recognized client-v0 kinds without a proper slice collection are rejected, not stored
  as future resources.
- Contaminated slices use `decode: 'tolerant'` and
  `unknown: [{ pointer, known_value? }]`. Each exact pointer supplies a strict-valid repair
  witness; omit `known_value` to remove an extra object key. The decode gate requires each
  declared contamination to fail strict decoding independently, and rejects undeclared errors.
  Declarations cannot repair a known object's ancestor to hide descendant errors. Whole
  resource/entry witnesses require a genuinely unknown union discriminator.
- `open-hold` keeps subsequent socket opens pending without any open/error/close callback;
  `open-release` resumes pending opens at the current clock instant. Already-open sockets
  are unaffected. Both are sync timeline events carrying `at_ms` and `store`.
- Socket `open-fail { opens: 'all' }` persists until `open-ok`; a positive integer fails that
  many opens (omission means one). Subscription `error { repeat: true, ... }` rejects each
  resubscribe until `error-clear`. HTTP faults accept
  `when: { cursor: 'present' | 'absent', query?: Record<string, string> }`; use a present cursor
  to expire older pages without breaking the first page. `http-ok` clears the matching policy.
- Capability omission alone does not reject attach/subscribe on the server, so replay adds no
  synthetic rejection. The capability-absent SDK expectation starts as `shape` and is pinned
  when the web consumer adopts it.
- Add variants in `src/kit/variants/<slice>.ts`, exporting
  `<slice>Variants: VariantTable['<slice>']`; the central module composes them. Populated variants
  synthesize from the cast when default records are absent. `roster.huge` uses the full
  1,000-agent roster; `conversation.huge` uses the first cast agent's seeded history and bounded
  committed window described above.

- `roster.all-states` cycles states across existing cast agents; `conversation.long` has 80
  exchanges per agent; `attention.many` has 50 cards. Singleton `remote-only-mail` casts use
  protocol-valid agent loopback mail, not invented participants.
- Loading consumers render their loading state without displaying withheld backing records.
  Story checks use one representative visible world contrast per declared slice; replication
  metadata alone is not a visible contrast for a roster-name story.

| Command | Effect |
| --- | --- |
| `pnpm --filter @smalltalk/st3-scenarios emit` | rewrites `fixtures/scenarios` |
| `pnpm --filter @smalltalk/st3-scenarios emit:check` | fails when the committed files are stale |
| `pnpm --filter @smalltalk/st3-scenarios decode` | strict-decodes every variant of every world |
| `pnpm --filter @smalltalk/st3-scenarios scan` | privacy gate |
| `CI=1 pnpm --filter @smalltalk/st3-scenarios test` | unit and property tests |

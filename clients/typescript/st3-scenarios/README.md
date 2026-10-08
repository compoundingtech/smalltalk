# @smalltalk/st3-scenarios

Seeded, privacy-safe scenario worlds for every Smalltalk client, and the kit that builds and
replays them. [spec.md](spec.md) is the contract.

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

## Authoring contracts

- Casts use either `roles` or `agents: { key, role, name?, workspace?, branch? }[]`.
  Stable unique keys determine identities and seeded forks; semantic roles may repeat. Workspace
  overrides stay under `~/src/<project>/`. `agent` also supports `state: 'desired'` without a runtime.
- `thread-create {thread}` and `terminal-create {record}` introduce complete resources after
  offset zero; `thread-remove {agent}` and `terminal-remove {terminal}` remove membership.
  Use the shared `terminalRecord(ctx, member, run, startedMs)` factory, not a world-local helper.
- Future resource kinds live in the roster/details/attention state's optional `resources`
  array and in `changes.upserts`. Fold keeps them outside known-kind dispatch; replay sends
  the original encoded values over primary collection sockets and generic resources HTTP.
- Contaminated slices use `decode: 'tolerant'` and
  `unknown: [{ pointer, known_value? }]`. Each exact pointer supplies a strict-valid repair
  witness; omit `known_value` to remove an extra object key. The decode gate requires each
  declared contamination to fail strict decoding independently, and rejects undeclared errors.
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
  synthesize from the cast when default records are absent. No paged/sharded fixture encoding
  is defined here; the catalog's `huge` format remains a separate decision.

| Command | Effect |
| --- | --- |
| `pnpm --filter @smalltalk/st3-scenarios emit` | rewrites `fixtures/scenarios` |
| `pnpm --filter @smalltalk/st3-scenarios emit:check` | fails when the committed files are stale |
| `pnpm --filter @smalltalk/st3-scenarios decode` | strict-decodes every variant of every world |
| `pnpm --filter @smalltalk/st3-scenarios scan` | privacy gate |
| `CI=1 pnpm --filter @smalltalk/st3-scenarios test` | unit and property tests |

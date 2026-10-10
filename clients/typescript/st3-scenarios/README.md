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

| Command | Effect |
| --- | --- |
| `pnpm --filter @smalltalk/st3-scenarios emit` | rewrites `fixtures/scenarios` |
| `pnpm --filter @smalltalk/st3-scenarios emit:check` | fails when the committed files are stale |
| `pnpm --filter @smalltalk/st3-scenarios decode` | strict-decodes every variant of every world |
| `pnpm --filter @smalltalk/st3-scenarios scan` | privacy gate |
| `CI=1 pnpm --filter @smalltalk/st3-scenarios test` | unit and property tests |

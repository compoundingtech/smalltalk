// Seed layouts for stories: conversation-first daily driver and explicitly opened edge states.
// Every editor input names a subject of the fixture world (`src/fixtures/world.ts`).

import { Schema } from 'effect'

import { agents, ciRuns, decisionD12, missions, pullRequests } from '../../fixtures/world.ts'
import { SubjectAddress } from '../../resources/contract.ts'
import { subjectKey, initialState, type EditorEntry, type WorkbenchState } from '../state.ts'

const worldRefs = new Set([
  ...agents.flatMap((a) => [a.ref, a.terminal]),
  ...missions.map((m) => m.ref),
  ...pullRequests.map((p) => p.ref),
  ...ciRuns.map((c) => c.ref),
])

/** An editor entry for a world subject; fails loudly when a layout names a subject the world lacks. */
const entry = (input: {
  readonly ref: string
  readonly presentation: SubjectAddress['presentation']
}): EditorEntry => {
  const address = Schema.decodeSync(SubjectAddress)(input)
  if (!worldRefs.has(address.ref))
    throw new Error(`layout fixture names unknown subject ${address.ref}`)
  return { input: address }
}

const conversation = entry({ ref: 'agent/build-host-a/workbench-shell', presentation: 'detail' })
const mission = entry({ ref: decisionD12.mission, presentation: 'detail' })
const pullRequest = entry({
  ref: 'resource/github/acme/webfractal/pull/214',
  presentation: 'detail',
})
const terminal = entry({ ref: 'terminal/build-host-a/workbench-shell', presentation: 'detail' })
const draft = entry({ ref: decisionD12.raisedBy, presentation: 'detail' })
const offline = entry({ ref: 'agent/build-host-c/sdk-interop', presentation: 'detail' })
const weeklyCycle = entry({ ref: 'mission/platform/deps/weekly-cycle', presentation: 'detail' })

/** `MissionsUi` has an unsent composer draft (its answer to D12). */
export const fixtureDirty: ReadonlyArray<string> = [subjectKey(draft.input)]

const docks: WorkbenchState['docks'] = {
  primary: { visible: true, size: 272, activeView: 'navigation' },
  panel: { visible: false, size: 200, activeView: 'monitor.quota' },
}

/** Conversation fills the workspace; the canonical terminal is opened on demand. */
export const dailyDriver: WorkbenchState = {
  ...initialState(docks),
  groups: {
    g0: { editors: [conversation, mission, pullRequest], active: subjectKey(conversation.input) },
  },
  focusedGroup: 'g0',
}

/** The same session layout is used for each visual comparison. */
export const lookComparison: WorkbenchState = {
  ...dailyDriver,
  docks: {
    ...docks,
    primary: { ...docks.primary, activeView: 'navigation' },
    panel: { ...docks.panel, visible: false },
  },
  groups: {
    g0: { editors: [conversation], active: subjectKey(conversation.input) },
  },
}

/** Nothing open yet: first-run hints in the only group. */
export const emptyWorkbench: WorkbenchState = initialState({
  ...docks,
  panel: { ...docks.panel, visible: false },
})

/** Focus mode: docks collapsed, a nested split (conversation | terminal over mission). */
export const focusMode: WorkbenchState = {
  ...initialState({
    primary: { ...docks.primary, visible: false },
    panel: { ...docks.panel, visible: false },
  }),
  editorArea: {
    _tag: 'split',
    dir: 'row',
    children: [
      { _tag: 'group', id: 'g0' },
      {
        _tag: 'split',
        dir: 'col',
        children: [
          { _tag: 'group', id: 'g1' },
          { _tag: 'group', id: 'g2' },
        ],
        sizes: [0.6, 0.4],
      },
    ],
    sizes: [0.5, 0.5],
  },
  groups: {
    g0: { editors: [conversation], active: subjectKey(conversation.input) },
    g1: { editors: [terminal], active: subjectKey(terminal.input) },
    g2: { editors: [mission], active: subjectKey(mission.input) },
  },
  focusedGroup: 'g0',
  nextGroup: 3,
}

/** Tab overflow: one group holding more editors than fit. */
export const manyTabs: WorkbenchState = {
  ...initialState(docks),
  groups: {
    g0: {
      editors: [
        conversation,
        terminal,
        mission,
        draft,
        offline,
        entry({ ref: 'agent/build-host-a/gateway-schema', presentation: 'detail' }),
        entry({ ref: 'agent/build-host-b/cas-janitor', presentation: 'detail' }),
        entry({ ref: 'terminal/build-host-b/cas-janitor', presentation: 'detail' }),
        weeklyCycle,
        entry({
          ref: 'resource/github/acme/webfractal/actions/run/11873452',
          presentation: 'detail',
        }),
        pullRequest,
      ],
      active: subjectKey(weeklyCycle.input),
    },
  },
}

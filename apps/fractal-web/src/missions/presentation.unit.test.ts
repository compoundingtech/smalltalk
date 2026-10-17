import type * as St3 from '@smalltalk/st3-client/schema'
import type { DecodedAgent } from '@st3/sdk/effect'
import { DateTime, Option } from 'effect'
import { describe, expect, it } from 'vitest'

import { hiddenByDefault, presentMission, sortMissions } from './presentation.ts'

const at = DateTime.makeUnsafe('2026-10-14T10:00:00Z')

const step = (path: string, state: string, fields: Record<string, unknown> = {}) => ({
  id: `step-run/example/${path}`,
  path,
  state,
  since: at,
  attempt: 1,
  title: Option.none(),
  claimant: Option.none(),
  assignee: Option.some('agent/example/builder'),
  blocked_reason: Option.none(),
  last_progress: Option.none(),
  ...fields,
})

const mission = (steps: readonly ReturnType<typeof step>[], fields: Record<string, unknown> = {}): St3.Mission =>
  ({
    id: 'mission/example/nightly',
    kind: 'mission',
    title: 'Nightly report',
    state: 'running',
    runs: ['mission-run/example/nightly/1'],
    run_details: [{ id: 'mission-run/example/nightly/1', status: 'running', steps: Option.some(steps), outcome: Option.none() }],
    updated_at: at,
    ...fields,
  }) as unknown as St3.Mission

const agent = (fields: Record<string, unknown> = {}): DecodedAgent =>
  ({ id: 'agent/example/builder', name: 'builder', state: 'running', fault: Option.none(), next_work_id: Option.none(), upcoming_work_ids: [], current_work: [], ...fields }) as unknown as DecodedAgent

const word = (m: St3.Mission, agents: readonly DecodedAgent[] = [agent()], attention: readonly St3.Attention[] = []) =>
  presentMission({ mission: m, agents, attention }).word

describe('presentMission', () => {
  it('follows st3-ui-model\'s precedence, first match wins', () => {
    const gate = { attention_kind: 'human-gate', state: 'open', mission_id: 'mission/example/nightly', source_id: 'step-run/example/review' } as unknown as St3.Attention
    expect(word(mission([step('build', 'claimed')]), [agent()], [gate])).toBe('decision')
    expect(word(mission([], { runs: [], run_details: [] }))).toBe('not_started')
    expect(word(mission([step('build', 'blocked')]))).toBe('stalled')
    expect(word(mission([step('build', 'failed')]), [agent()], [])).toBe('failed')
    expect(word(mission([step('build', 'completed')], { state: 'completed' }))).toBe('done')
    expect(word(mission([step('build', 'claimed')]))).toBe('working')
    expect(word(mission([step('build', 'completed'), step('keep-watch', 'claimed', { agentless: true, keep_open: true })]))).toBe('watching')
    expect(word(mission([step('build', 'waiting')]))).toBe('held')
    expect(word(mission([step('build', 'completed')]))).toBe('idle')
  })

  it('says who has a ready step queued: unclaimed, queued, or unstaffed when its seat is stopped', () => {
    const ready = mission([step('build', 'ready')])
    expect(word(ready, [])).toBe('unclaimed')
    const queuing = { next_work_id: Option.some('step-run/example/build') }
    expect(word(ready, [agent(queuing)])).toBe('queued')
    expect(word(ready, [agent({ ...queuing, state: 'stopped' })])).toBe('unstaffed')
    expect(word(ready, [agent({ ...queuing, fault: Option.some('login expired') })])).toBe('unstaffed')
    const presented = presentMission({ mission: ready, agents: [agent({ ...queuing, state: 'stopped' })], attention: [] })
    expect(presented.who).toEqual({ kind: 'agent', agent: 'agent/example/builder', label: 'builder' })
    expect(presented.stuck).toMatchObject({ step: 'step-run/example/build', agent: { id: 'agent/example/builder', label: 'builder', state: 'stopped' } })
    expect(presented.steps[0]?.note).toBe('queued for builder, which is stopped')
  })

  it('orders steps like a pipeline, names their owners and counts progress', () => {
    const gated = { attention_kind: 'human-gate', state: 'open', mission_id: 'mission/other', source_id: 'step-run/example/review' } as unknown as St3.Attention
    const presented = presentMission({
      mission: mission([
        step('deploy', 'pending'),
        step('review', 'waiting'),
        step('build', 'claimed', { claimant: Option.some('agent/example/builder'), last_progress: Option.some('linking') }),
        step('fetch', 'completed', { agentless: true }),
      ]),
      agents: [agent()],
      attention: [gated],
    })
    expect(presented.steps.map((each) => [each.path, each.word, each.owner.label])).toEqual([
      ['fetch', 'done', 'st'], ['review', 'needs_you', 'you'], ['build', 'working', 'builder'], ['deploy', 'waiting', 'builder'],
    ])
    expect(presented.progress).toEqual({ done: 1, total: 4 })
    expect(presented.last_progress).toBe('linking')
    expect(presented.who).toEqual({ kind: 'agent', agent: 'agent/example/builder', label: 'builder' })
  })

  it('reads the steps of the open runs, else the latest run', () => {
    const runs = [
      { id: 'mission-run/example/nightly/1', status: 'completed', steps: Option.some([step('old', 'completed')]), outcome: Option.none() },
      { id: 'mission-run/example/nightly/2', status: 'running', steps: Option.some([step('new', 'claimed')]), outcome: Option.none() },
    ]
    expect(presentMission({ mission: mission([], { run_details: runs }), agents: [], attention: [] }).steps.map((each) => each.path)).toEqual(['new'])
    expect(presentMission({ mission: mission([], { run_details: [runs[0], { ...runs[1], status: 'failed' }] }), agents: [], attention: [] }).steps.map((each) => each.path)).toEqual(['new'])
  })

  it('folds away st\'s own, never started and settled-before-today missions, and sorts by word', () => {
    const today = Date.parse('2026-10-14T00:00:00Z')
    const yesterday = DateTime.makeUnsafe('2026-10-13T10:00:00Z')
    const settledYesterday = presentMission({ mission: mission([step('build', 'completed')], { state: 'completed', updated_at: yesterday }), agents: [], attention: [] })
    expect(hiddenByDefault(settledYesterday, today)).toBe(true)
    expect(hiddenByDefault(presentMission({ mission: mission([step('build', 'claimed')], { system: true }), agents: [], attention: [] }), today)).toBe(true)
    expect(hiddenByDefault(presentMission({ mission: mission([step('build', 'claimed')]), agents: [], attention: [] }), today)).toBe(false)
    const rows = ['done', 'working', 'decision', 'stalled'].map((each) => ({ id: each, presentation: { word: each } as never }))
    expect(sortMissions(rows).map((row) => row.id)).toEqual(['decision', 'stalled', 'working', 'done'])
  })
})

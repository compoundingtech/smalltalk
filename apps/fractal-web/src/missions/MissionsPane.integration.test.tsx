// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
import { DateTime, Option } from 'effect'

const data = vi.hoisted(() => ({ missions: { _tag: 'Waiting' } as unknown, agents: { _tag: 'Observed', value: [], freshness: 'live' } as unknown, attention: { _tag: 'Observed', value: [], freshness: 'live' } as unknown }))
vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'test-animation', props: () => ({}) }))
vi.mock('@effect/atom-react', () => ({ useAtomValue: (atom: unknown) => atom }))
vi.mock('../data/react.tsx', () => ({
  useDataSource: () => ({ missions: data.missions, agents: data.agents, attention: data.attention }),
  useNow: () => Date.parse('2026-10-14T12:00:00Z'),
}))

import { MissionsPane } from './MissionsPane.tsx'

const at = DateTime.makeUnsafe('2026-10-14T11:00:00Z')
const mission = (id: string, title: string, state: string, steps: unknown[], fields: Record<string, unknown> = {}) => ({
  id, kind: 'mission', title, state, runs: [`${id}/run`], updated_at: at,
  run_details: [{ id: `${id}/run`, status: state === 'completed' ? 'completed' : 'running', steps: Option.some(steps), outcome: Option.none() }],
  ...fields,
})
const step = (path: string, state: string, claimant?: string) => ({
  id: `step-run/${path}`, path, state, since: at, attempt: 1, title: Option.some(`Do ${path}`),
  claimant: claimant === undefined ? Option.none() : Option.some(claimant), assignee: Option.none(), blocked_reason: Option.none(), last_progress: Option.none(),
})

it('lists missions by word with who must act, folds away st\'s own, and opens a mission to its steps', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const onOpenAgent = vi.fn()
  await act(async () => root.render(<MissionsPane onOpenAgent={onOpenAgent} />))
  expect(container.querySelector('[data-system-state="loading"]')?.textContent).toBe('Loading missions')
  data.agents = { _tag: 'Observed', value: [{ id: 'agent/example/builder', name: 'builder', state: 'running', fault: Option.none(), next_work_id: Option.none(), upcoming_work_ids: [], current_work: [] }], freshness: 'live' }
  data.missions = { _tag: 'Observed', freshness: 'live', value: [
    mission('mission/example/build', 'Build the site', 'running', [step('compile', 'claimed', 'agent/example/builder'), step('ship', 'pending')]),
    mission('mission/example/loop', 'st loop', 'running', [step('tick', 'claimed', 'agent/example/builder')], { system: true }),
    mission('mission/example/stuck', 'Fix the parser', 'running', [step('parse', 'blocked', 'agent/example/builder')]),
  ] }
  await act(async () => root.render(<MissionsPane onOpenAgent={onOpenAgent} />))
  const rows = [...container.querySelectorAll('li > button')].map((row) => row.textContent)
  expect(rows).toEqual(['▲stalledFix the parserbuilder · 0/1 · 1h', '⠋workingBuild the sitebuilder · 0/2 · 1h'])
  expect(container.textContent).toContain('Show all (1 more)')
  await act(async () => (container.querySelector('li > button') as HTMLElement).click())
  await act(async () => [...container.querySelectorAll('button')].find((each) => each.textContent === 'Show all (1 more)')!.click())
  expect([...container.querySelectorAll('li > button')].map((row) => row.textContent?.includes('st loop'))).toContain(true)
  const detail = container.querySelector('[aria-label="Fix the parser steps"]')!
  expect(detail.textContent).toContain('a step has an owner who is not moving')
  expect(detail.textContent).toContain('Do parse')
  await act(async () => [...detail.querySelectorAll('button')].find((each) => each.textContent === 'builder')!.click())
  expect(onOpenAgent).toHaveBeenCalledWith('agent/example/builder')
  act(() => root.unmount())
  container.remove()
})

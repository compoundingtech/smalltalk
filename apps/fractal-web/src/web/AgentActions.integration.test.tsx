// @vitest-environment jsdom
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { describe, expect, it, vi } from 'vitest'
import type { St3Client } from '@smalltalk/st3-client'

const data = vi.hoisted(() => {
  const control = vi.fn(async (_agent: string, _control: string) => ({ _tag: 'Sent' as const }))
  const interrupt = vi.fn(async (_agent: string) => ({ _tag: 'Sent' as const }))
  return { control, interrupt, source: { agentControls: { control, interrupt } } }
})
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../data/react.tsx', () => ({ useDataSource: () => data.source }))

import { gatewayAgentControls } from '../agents/agent-control-port.ts'
import { AgentActions, agentActions } from './AgentActions.tsx'

const keys = (state: string, activity: 'working' | 'idle' = 'idle') => agentActions({ state, activity }).map((row) => row.key)

describe('agentActions', () => {
  it('offers what stui offers for the seat\'s state, in its order', () => {
    expect(keys('running', 'working')).toEqual(['interrupt', 'restart', 'suspend', 'retire', 'copy'])
    expect(keys('running')).toEqual(['restart', 'suspend', 'retire', 'copy'])
    expect(keys('starting')).toEqual(['restart', 'suspend', 'retire', 'copy'])
    expect(keys('stopped')).toEqual(['start', 'resume', 'copy'])
    expect(keys('suspended')).toEqual(['start', 'resume', 'copy'])
    expect(agentActions({ state: 'running', activity: 'working' }).filter((row) => row.confirm).map((row) => row.key))
      .toEqual(['restart', 'suspend', 'retire'])
  })
})

describe('gatewayAgentControls', () => {
  const envelope = <T,>(value: T, snapshot = 'snapshot/now') => ({ snapshot: { id: snapshot }, value })
  const fake = () => {
    const calls: Array<[string, Record<string, unknown>]> = []
    const record = (name: string) => vi.fn(async (input: Record<string, unknown>) => { calls.push([name, input]); return envelope({ status: 'completed' }) })
    let token = 0
    const client = {
      subjectDefinition: vi.fn(async () => envelope({ desired_token: `desired-${++token}` }, `snapshot/${token}`)),
      agentsGet: vi.fn(async () => envelope({ kind: 'agent', runtime_ids: ['runtime/atlas'] })),
      runtimesGet: vi.fn(async () => envelope({ kind: 'runtime', incarnation_id: 'incarnation/7', terminal_id: 'terminal/atlas' })),
      terminalScreen: vi.fn(async () => envelope({ runtime_incarnation: 'incarnation/7', next_sequence: 42 }, 'snapshot/screen')),
      agentStop: record('agent.stop'), agentStart: record('agent.start'), agentSuspend: record('agent.suspend'), agentResume: record('agent.resume'), terminalInput: record('terminal.input'),
    }
    return { calls, client, port: gatewayAgentControls(client as unknown as St3Client) }
  }

  it('restarts as a stop and then a start, each fenced on the declaration st selects then', async () => {
    const { calls, port } = fake()
    expect(await port.control('agent/example/atlas', 'restart')).toEqual({ _tag: 'Sent' })
    expect(calls.map(([name, input]) => [name, input.fence, input.parameters])).toEqual([
      ['agent.stop', { snapshot_id: 'snapshot/1', subject_revisions: {}, runtime_desired_revision: 'desired-1' }, { agent: 'agent/example/atlas', reason: "from the web app's agent actions" }],
      ['agent.start', { snapshot_id: 'snapshot/2', subject_revisions: {}, runtime_desired_revision: 'desired-2' }, { agent: 'agent/example/atlas' }],
    ])
  })

  it('says the seat was left stopped when a restart\'s start fails after its stop', async () => {
    const { calls, port, client } = fake()
    client.agentStart.mockRejectedValueOnce(new TypeError('network down'))
    expect(await port.control('agent/example/atlas', 'restart')).toEqual({
      _tag: 'Refused',
      detail: 'Stopped, but starting it again failed: st did not confirm it; check the agent before trying again. It was not sent again. Start it from this menu.',
    })
    expect(calls.map(([name]) => name)).toEqual(['agent.stop'])
    // A refused stop leaves it running, and says only why.
    const second = fake()
    second.client.agentStop.mockRejectedValueOnce(new TypeError('network down'))
    expect(await second.port.control('agent/example/atlas', 'restart')).toEqual({
      _tag: 'Refused', detail: 'st did not confirm it; check the agent before trying again. It was not sent again.',
    })
  })

  it('fences a suspension on the running incarnation too', async () => {
    const { calls, port } = fake()
    await port.control('agent/example/atlas', 'suspend')
    expect(calls[0]).toEqual(['agent.suspend', expect.objectContaining({
      fence: { snapshot_id: 'snapshot/1', subject_revisions: {}, runtime_desired_revision: 'desired-1', runtime_incarnation: 'incarnation/7' },
    })])
  })

  it('stops a turn with Esc to the agent\'s terminal, fenced on its screen', async () => {
    const { calls, port } = fake()
    expect(await port.interrupt('agent/example/atlas')).toEqual({ _tag: 'Sent' })
    expect(calls[0]).toEqual(['terminal.input', expect.objectContaining({
      fence: { snapshot_id: 'snapshot/screen', subject_revisions: {}, runtime_incarnation: 'incarnation/7', terminal_sequence: 42 },
      parameters: { terminal_id: 'terminal/atlas', mode: 'key', value: 'escape' },
    })])
  })
})

describe('AgentActions', () => {
  const mount = async (state: string) => {
    vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
    // React Aria's menu selection escapes keys with CSS.escape, which jsdom does not provide.
    if (globalThis.CSS?.escape === undefined) vi.stubGlobal('CSS', { escape: (value: string) => value.replace(/[^\w-]/g, (char) => `\\${char}`) })
    const container = document.createElement('div')
    document.body.append(container)
    const root = createRoot(container)
    await act(async () => root.render(<AgentActions agent={{ ref: 'agent/example/atlas', name: 'atlas', state, activity: 'idle' }} />))
    const press = async (label: string) => act(async () => {
      // Icon-only buttons are found by their accessible name; text buttons and menu items by their text.
      const target = [...document.querySelectorAll<HTMLElement>('button, [role="menuitem"]')].find((each) => (each.getAttribute('aria-label') ?? each.textContent)?.startsWith(label))
      if (target === undefined) throw new Error(`no ${label}`)
      target.click()
    })
    return { press, unmount: () => { act(() => root.unmount()); container.remove() } }
  }

  it('asks before restarting, and keeping it sends nothing', async () => {
    data.control.mockClear()
    const view = await mount('running')
    await view.press('Actions for atlas')
    await view.press('Restart')
    expect(document.querySelector('[role="alertdialog"]')?.textContent).toContain('Restart atlas?')
    expect(data.control).not.toHaveBeenCalled()
    await view.press('Keep it')
    expect(document.querySelector('[role="alertdialog"]')).toBeNull()
    await view.press('Actions for atlas')
    await view.press('Restart')
    await act(async () => [...document.querySelectorAll<HTMLElement>('[role="alertdialog"] button')].find((each) => each.textContent === 'Restart')!.click())
    expect(data.control).toHaveBeenCalledWith('agent/example/atlas', 'restart')
    expect(document.querySelector('[role="status"]')?.textContent).toBe('Restart atlas: asked st')
    view.unmount()
  })

  it('starts a stopped seat without asking', async () => {
    data.control.mockClear()
    const view = await mount('stopped')
    await view.press('Actions for atlas')
    await view.press('Start')
    expect(data.control).toHaveBeenCalledWith('agent/example/atlas', 'start')
    expect(document.querySelector('[role="alertdialog"]')).toBeNull()
    view.unmount()
  })
})

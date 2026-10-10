// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Agent } from '../data/source.ts'
import type { SubjectSummary } from '../shell/context.tsx'
import { sidebarAttention } from './sidebarAttention.ts'
import { faviconDotColor, faviconWorkingColor, selectedFaviconState, tabTitle, useTabMetadata, type FaviconState } from './useTabMetadata.ts'

vi.mock('@stylexjs/stylex', () => ({ defineVars: (values: unknown) => values }))
const agent: Agent = { ref: 'agent/example', terminal: '', name: 'Example', lifecycle: { _tag: 'Unknown' }, host: 'host/example', connected: true, activity: 'working', status: 'working', state: 'running', usage: { _tag: 'Unknown' }, checkout: { _tag: 'Unknown' }, workspace: { _tag: 'Unknown' }, startedAt: { _tag: 'Unknown' }, endedAt: { _tag: 'Unknown' }, blockedOn: { _tag: 'Unknown' }, ask: { _tag: 'Unknown' }, lastActivityAt: { _tag: 'Unknown' } }
let root: Root
let host: HTMLDivElement
let icon: HTMLLinkElement
let paints: (string | undefined)[]
let dot: string | undefined
const canvas = { fillStyle: '', strokeStyle: '', lineWidth: 0, beginPath() {}, roundRect() {}, fillRect() {}, stroke() {}, arc() { dot = canvas.fillStyle }, fill() { if (dot !== undefined) dot = canvas.fillStyle } }

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  document.title = 'Fractal'
  host = document.createElement('div')
  host.dataset.testid = 'live-agent-workspace'
  document.body.append(host)
  icon = document.createElement('link')
  icon.rel = 'icon'
  document.head.append(icon)
  root = createRoot(host)
  paints = []
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockImplementation(() => { dot = undefined; return canvas as unknown as CanvasRenderingContext2D })
  vi.spyOn(HTMLCanvasElement.prototype, 'toDataURL').mockImplementation(() => { paints.push(dot); return `data:image/png;base64,${paints.length}` })
})
afterEach(async () => {
  await act(async () => root.unmount())
  host.remove()
  icon.remove()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})
const Metadata = (props: { title: string; faviconState: FaviconState }) => { useTabMetadata(props); return null }
const render = async (title: string, faviconState: FaviconState = 'none') => {
  await act(async () => root.render(<Metadata title={title} faviconState={faviconState} />))
}

describe('reactive Fractal tab', () => {
  it.each([
    [undefined, 0, false, 'Fractal'], [undefined, 3, true, 'Fractal'],
    ['Example', 0, false, 'Example · Fractal'], ['Example', 2, false, '(2) Example · Fractal'],
    ['Example', 0, true, 'Offline · Example · Fractal'], ['Example', 2, true, 'Offline · Example · Fractal'],
    ['<b>Example</b>', 1, false, '(1) <b>Example</b> · Fractal'],
  ])('writes plain title for %s / %i / offline=%s', async (agentName, needsYouCount, offline, expected) => {
    await render(tabTitle({ agentName, needsYouCount, offline }))
    expect(document.title).toBe(expected)
    expect(document.head.querySelector('b')).toBeNull()
  })
  it('updates on an agent switch in the same commit, with one render and no later render', async () => {
    const seen: string[] = []
    const ReadTitle = () => { React.useLayoutEffect(() => { seen.push(document.title) }); return null }
    const renders = { pane: 0, sidebar: 0 }
    const Pane = () => { renders.pane++; return null }
    const Sidebar = () => { renders.sidebar++; return null }
    const Shell = ({ name }: { name: string }) => <><Metadata title={tabTitle({ agentName: name, needsYouCount: 0, offline: false })} faviconState="working" /><Pane /><Sidebar /><ReadTitle /></>
    await act(async () => root.render(<Shell name="Example" />))
    expect(renders).toEqual({ pane: 1, sidebar: 1 })
    await act(async () => root.render(<Shell name="Other" />))
    expect(seen).toEqual(['Example · Fractal', 'Other · Fractal'])
    expect(renders).toEqual({ pane: 2, sidebar: 2 })
    expect(paints).toHaveLength(1)
  })
  it('does not write or redraw unchanged values, including StrictMode effect replay', async () => {
    const writes = vi.spyOn(document, 'title', 'set')
    await act(async () => root.render(<React.StrictMode><Metadata title="Example · Fractal" faviconState="working" /></React.StrictMode>))
    expect(paints).toHaveLength(1)
    await render('Example · Fractal', 'working')
    // Removing StrictMode remounts; the same title still must not be written again.
    expect(writes).toHaveBeenCalledTimes(1)
    const drawCount = paints.length
    await render('Example · Fractal', 'working')
    expect(writes).toHaveBeenCalledTimes(1)
    expect(paints).toHaveLength(drawCount)
  })
  it.each([
    ['working', 'rgb(34, 197, 94)'], ['needs-you', 'rgb(254, 154, 0)'], ['error', 'rgb(251, 65, 74)'], ['none', undefined],
  ] as const)('resolves the %s theme color', (state, expected) => {
    const color = faviconDotColor({ state, themeRoot: host })
    if (state === 'working') expect(color).toBe(faviconWorkingColor)
    else expect(color).toBe(expected)
    expect(host.children).toHaveLength(0)
  })
  it('paints amber and red on the Fractal icon, and writes the link only when its state changes', async () => {
    const writes = vi.spyOn(icon, 'href', 'set')
    await render('Example · Fractal', 'needs-you')
    expect(paints).toEqual(['rgb(254, 154, 0)'])
    await render('(2) Example · Fractal', 'needs-you')
    expect(writes).toHaveBeenCalledTimes(1)
    await render('(2) Example · Fractal', 'error')
    expect(paints).toEqual(['rgb(254, 154, 0)', 'rgb(251, 65, 74)'])
    expect(writes).toHaveBeenCalledTimes(2)
  })
  it.each([
    [{ activity: 'working', state: 'running' }, false, 'working'],
    [{ activity: 'idle', state: 'running' }, true, 'needs-you'],
    [{ activity: 'errored', state: 'failed' }, false, 'error'],
    [{ activity: 'idle', state: 'idle' }, false, 'none'],
    [{ activity: 'working', state: 'done' }, false, 'none'],
    [{ activity: 'waiting', state: 'waiting' }, false, 'none'],
    [{ activity: 'working', state: 'unknown' }, false, 'none'],
    [{ activity: 'working', state: undefined }, true, 'none'],
    [{ activity: 'working', connected: false }, true, 'none'],
  ] as const)('maps selected native state %o (attention %s)', (change, needsYou, expected) => {
    expect(selectedFaviconState({ agent: { ...agent, ...change }, needsYou, offline: false })).toBe(expected)
  })
  it('never retains a working dot after done, offline or clearing selection', async () => {
    await render('Example · Fractal', 'working')
    expect(paints).toEqual([faviconWorkingColor])
    await render('Example · Fractal', selectedFaviconState({ agent: { ...agent, state: 'done' }, needsYou: false, offline: false }))
    expect(paints).toEqual([faviconWorkingColor, undefined])
    expect(icon.type).toBe('image/png')
    expect(icon.href).toBe('data:image/png;base64,2')
    expect(selectedFaviconState({ agent, needsYou: true, offline: true })).toBe('none')
    expect(selectedFaviconState({ needsYou: true, offline: false })).toBe('none')
  })
  it('counts distinct roster agents from exactly the shared sidebar attention derivation', async () => {
    const subject = (ref: string, attention?: boolean, icon: SubjectSummary['icon'] = 'conversation'): SubjectSummary => ({ ref, title: ref, icon, ...(attention === undefined ? {} : { attention }) })
    const agents = [agent, { ...agent, ref: 'agent/other' }, { ...agent, ref: 'agent/unknown' }]
    const subjects = [subject(agent.ref, true), subject(agent.ref, true), subject('agent/other', true), subject('agent/unknown'), subject('agent/absent', true), subject('mission/example', true, 'missions'), subject('terminal/example', true, 'terminal')]
    const needsYou = sidebarAttention({ agents, subjects })
    expect([...needsYou]).toEqual([agent.ref, 'agent/other'])
    await render(tabTitle({ agentName: agent.name, needsYouCount: needsYou.size, offline: false }))
    expect(document.title).toBe('(2) Example · Fractal')
    expect(sidebarAttention({ agents, subjects: [] }).size).toBe(0)
  })
})

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { useArgs } from 'storybook/preview-api'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { SwitchingLane, laneId, switcherNames, variants, type SwitchingArgs } from './SubagentSwitching'
import { activeCountName, activeSubagents, alertWords, scenarios, unreportedExplanation, workspaces, sidebarFixtures, type Agent, type Scenario } from './switching-fixtures'
import { baselineTheme } from '../assistant-ui/neutral-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t, geometryVars as g, borderVars as border, radiusVars as r, accentVars as accent } from '../assistant-ui/composition-tokens.stylex'

interface Choice { readonly id: string; readonly label?: string; readonly description: string }
const switchChoices: readonly Choice[] = [
  { id: 'S1', description: `${switcherNames.S1}: one chip per thread under the composer, like coding-agent CLIs. Overflow opens the thread drawer; a count press focuses the open thread’s chip.` },
  { id: 'S2', description: `${switcherNames.S2}: one tab per thread above the conversation. Overflow opens the thread drawer; a count press focuses the selected tab, so arrow keys move at once.` },
  { id: 'S3', description: `${switcherNames.S3}: the path to the open thread, plus a drawer that lists every thread as a tree. A count press focuses the drawer trigger.` },
]
const placementChoices: readonly Choice[] = [
  { id: 'chip', label: 'P1', description: 'Count tag right after the name, read with the row’s metadata. Nothing at zero; “? subagents” when the harness does not report subagents.' },
  { id: 'badge', label: 'P2', description: 'Numeric badge at the trailing edge, beside the status disc. Nothing at zero; “?” and the subagent glyph when the harness does not report subagents.' },
]
const widthChoices: readonly Choice[] = [{ id: '240', label: 'W1', description: '240 px: compact sidebar; every row stays on one line.' }, { id: '320', label: 'W2', description: '320 px: default-width sidebar.' }]
const scenarioChoices: readonly Choice[] = scenarios.map(state => {
  const fixture = workspaces[state]
  const summary = fixture.reporting === 'unreported' ? `${fixture.harness} does not report subagents, so the count is unknown, never 0.` : `${activeCountName(fixture)}. Counts include running and waiting subagents, never finished or failed ones.`
  return { id: state, description: `${fixture.label}: ${summary}${fixture.sample ? ' Sample: st does not report these child states until #2288.' : ''}` }
})
/** One picker row per dimension: short IDs, then a one-line description of the current pick. */
function Picker({ label, choices, value, onChange }: { label: string; choices: readonly Choice[]; value: string; onChange: (id: string) => void }) {
  const id = React.useId()
  return <div role="group" aria-labelledby={id} {...stylex.props(styles.picker)}>
    <span id={id} {...stylex.props(styles.pickerLabel)}>{label}</span>
    <div {...stylex.props(styles.pickerLine)}>
      {choices.map(choice => <Button key={choice.id} aria-pressed={choice.id === value} onPress={() => onChange(choice.id)} {...stylex.props(styles.button, choice.id === value && styles.selected)}>{choice.label ?? choice.id}</Button>)}
      <span data-picker-description {...stylex.props(styles.description)}>{choices.find(choice => choice.id === value)?.description}</span>
    </div>
  </div>
}
function Toggle({ pressed, onPress, children }: { pressed: boolean; onPress: () => void; children: React.ReactNode }) {
  return <Button aria-pressed={pressed} onPress={onPress} {...stylex.props(styles.button, pressed && styles.selected)}>{children}</Button>
}
/** A single lane is scaled down to fit the page and says so, instead of clipping the nominal 1440 app region; compare views scroll horizontally. */
function Fit({ width, children }: { width: number; children: React.ReactNode }) {
  const outer = React.useRef<HTMLDivElement>(null)
  const [scale, setScale] = React.useState(1)
  React.useLayoutEffect(() => {
    const element = outer.current!
    const measure = () => setScale(Math.min(1, element.clientWidth / width))
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [width])
  const percent = Math.round(scale * 100)
  return <div ref={outer} data-fit-scale={percent}>
    {scale < 1 ? <p data-fit-note {...stylex.props(styles.fitNote)}>Shown at {percent}% so the whole {width} px app region fits this page.</p> : null}
    <div {...stylex.props(styles.previewRow, scale < 1 && styles.zoom(String(scale)))}>{children}</div>
  </div>
}
const argKeys = ['switcher', 'placement', 'sidebarWidth', 'state', 'compare', 'allLanes', 'scheme', 'width', 'focusRequest', 'focusLane'] as const satisfies readonly (keyof SwitchingArgs)[]
/** Storybook's `args` URL syntax; also the identity used to recognise Storybook echoing an update back. */
const argsLine = (args: SwitchingArgs) => argKeys.map(key => `${key}:${typeof args[key] === 'boolean' ? `!${args[key]}` : args[key]}`).join(';')
function Explorer({ probe, onUpdate, ...incoming }: SwitchingArgs & { probe: boolean; onUpdate?: (next: SwitchingArgs) => void }) {
  // Every press renders from React state in its own commit, so a count press selects the seat and focuses its switcher before the frame ends.
  // Storybook args and the URL follow for sharing; their round trip is async and slow on the first update after load.
  const [args, setArgs] = React.useState<SwitchingArgs>(incoming)
  const sent = React.useRef<string[]>([])
  const incomingLine = argsLine(incoming)
  React.useLayoutEffect(() => {
    const echo = sent.current.indexOf(incomingLine)
    if (echo >= 0) {
      sent.current.splice(0, echo + 1)
      return
    }
    // A change from the Controls panel or the URL, not an echo of a press.
    setArgs(current => argsLine(current) === incomingLine ? current : incoming)
  }, [incomingLine])
  const update = (patch: Partial<SwitchingArgs>) => {
    const next = { ...args, ...patch }
    setArgs(next)
    if (probe || onUpdate === undefined) return
    sent.current.push(argsLine(next))
    onUpdate(next)
  }
  const schemes = args.allLanes ? ['dark', 'light'] as const : [args.scheme]
  const widths = args.allLanes ? [1440, 900] as const : [args.width]
  const switchers = args.compare ? variants : [args.switcher]
  const multiple = args.allLanes || args.compare
  return <main data-switching-explorer {...stylex.props(...baselineTheme, styles.root)}>
    <h1 {...stylex.props(styles.title)}>Subagent switching</h1>
    <p {...stylex.props(styles.lede)}>Pick a switcher and where the sidebar shows each agent’s active subagents. The sidebar rows and every switcher read the same fixture. Pressing a count opens that agent with its switcher focused. Child states and child conversations are labelled samples until st reports them (#2288).</p>
    <div {...stylex.props(styles.pickers)}>
      <Picker label="S · Switcher" choices={switchChoices} value={args.switcher} onChange={switcher => update({ switcher: switcher as SwitchingArgs['switcher'], focusRequest: 0, focusLane: '' })} />
      <Picker label="P · Active count" choices={placementChoices} value={args.placement} onChange={placement => update({ placement: placement as SwitchingArgs['placement'] })} />
      <Picker label="W · Sidebar width" choices={widthChoices} value={String(args.sidebarWidth)} onChange={sidebarWidth => update({ sidebarWidth: Number(sidebarWidth) as SwitchingArgs['sidebarWidth'] })} />
      <Picker label="Scenario" choices={scenarioChoices} value={args.state} onChange={state => update({ state: state as Scenario, focusRequest: 0, focusLane: '' })} />
    </div>
    <div role="group" aria-label="View" {...stylex.props(styles.view)}>
      <span {...stylex.props(styles.pickerLabel)}>View</span>
      <div {...stylex.props(styles.segment)}>{(['dark', 'light'] as const).map(scheme => <Toggle key={scheme} pressed={!args.allLanes && args.scheme === scheme} onPress={() => update({ scheme, allLanes: false, focusRequest: 0, focusLane: '' })}>{scheme}</Toggle>)}</div>
      <div {...stylex.props(styles.segment)}>{([1440, 900] as const).map(width => <Toggle key={width} pressed={!args.allLanes && args.width === width} onPress={() => update({ width, allLanes: false, focusRequest: 0, focusLane: '' })}>{width} wide</Toggle>)}</div>
      <div {...stylex.props(styles.segment)}>
        <Toggle pressed={args.compare} onPress={() => update({ compare: !args.compare })}>Compare S1–S3</Toggle>
        <Toggle pressed={args.allLanes} onPress={() => update({ allLanes: !args.allLanes })}>Light + dark × 1440 + 900</Toggle>
      </div>
    </div>
    <div {...stylex.props(styles.matrix)}>{schemes.flatMap(scheme => widths.map(width => {
      const lanes = switchers.map(switcher => <SwitchingLane key={`${switcher}/${args.state}/${scheme}/${width}`} {...args} switcher={switcher} scheme={scheme} width={width} landmarkContext={multiple ? `${switcher} ${scheme} ${width}` : undefined} onOpenWorkspace={(state, focusSwitcher) => update(focusSwitcher ? { state, focusRequest: args.focusRequest + 1, focusLane: laneId({ switcher, scheme, width }) } : { state, focusRequest: 0, focusLane: '' })} />)
      return multiple ? <div key={`${scheme}/${width}`} {...stylex.props(styles.previewRow)}>{lanes}</div> : <Fit key={`${scheme}/${width}`} width={width}>{lanes}</Fit>
    }))}</div>
  </main>
}
const meta = {
  title: 'Fractal UI/Explore/SubagentSwitching', component: Explorer,
  render: args => {
    const [, updateArgs] = useArgs<SwitchingArgs>()
    // Picks live in the story args and the URL, so every combination is a shareable link, also from the bare iframe.
    const update = (next: SwitchingArgs) => {
      updateArgs(next)
      const url = new URL(window.location.href)
      url.searchParams.set('args', argsLine(next))
      window.history.replaceState(window.history.state, '', url)
    }
    return <Explorer {...args} onUpdate={update} />
  },
  parameters: { layout: 'fullscreen' },
  args: { switcher: 'S2', placement: 'chip', sidebarWidth: 240, state: 'running', compare: false, allLanes: false, scheme: 'dark', width: 1440, focusRequest: 0, focusLane: '', probe: false },
  argTypes: { switcher: { options: variants, control: 'radio' }, placement: { options: ['chip', 'badge'], control: 'radio' }, sidebarWidth: { options: [240, 320], control: 'radio' }, state: { options: scenarios, control: 'select' }, compare: { control: 'boolean' }, allLanes: { control: 'boolean' }, scheme: { options: ['dark', 'light'], control: 'radio' }, width: { options: [1440, 900], control: 'radio' }, focusRequest: { table: { disable: true } }, focusLane: { table: { disable: true } }, probe: { table: { disable: true } }, onUpdate: { table: { disable: true } } },
} satisfies Meta<typeof Explorer>
export default meta
type Story = StoryObj<typeof meta>
export const Exploration: Story = {}

/** One line: every text box of the name and the count sits on the same line (an ellipsis may split a truncated name's boxes, never its line), nothing overflows, and the row stays 32px. */
function singleLine(row: HTMLElement): boolean {
  return [...row.querySelectorAll('[data-row-name], [data-count-text]')].every(element => {
    const range = document.createRange()
    range.selectNodeContents(element)
    const rects = [...range.getClientRects()]
    return rects.length > 0 && rects.every(rect => Math.abs(rect.top - rects[0]!.top) < 1)
  }) && row.scrollWidth <= row.clientWidth && row.getBoundingClientRect().height <= 32
}
/**
 * Reported: the visible number equals the fixture's active subagents, its text starts the spoken name, and the name is the fixture's words; zero shows no control.
 * Unreported: no digit anywhere in the row, the "? subagents" / "?" mark, and the unavailable sentence as the control's name.
 */
function countMatches(row: HTMLElement, state: Scenario): boolean {
  const fixture = workspaces[state]
  const name = activeCountName(fixture)
  if (fixture.reporting === 'unreported') {
    const unknown = row.querySelector<HTMLElement>('[data-count-unknown]')
    const mark = row.getAttribute('data-placement') === 'chip' ? '? subagents' : '?'
    return unknown !== null && row.querySelector('[data-active-count]') === null && !/\d/.test(row.textContent ?? '') && unknown.querySelector('[data-count-text]')?.textContent === mark && within(row).queryByRole('button', { name }) === unknown
  }
  const count = row.querySelector<HTMLElement>('[data-active-count]')
  const expected = activeSubagents(fixture)
  if (expected === 0) return count === null && row.querySelector('[data-count-unknown]') === null
  const visible = row.querySelector('[data-count-text]')?.textContent ?? ''
  return Number.parseInt(visible, 10) === expected && name.startsWith(visible) && within(row).queryByRole('button', { name }) === count
}
function switcherFocused(lane: HTMLElement): boolean {
  const focused = document.activeElement
  return focused !== null && lane.contains(focused) && focused.closest('[data-switcher]') !== null
}
/** M1: focus sits on the lane's selected tab, so the next arrow key moves between threads. */
function selectedTabFocused(lane: HTMLElement): boolean {
  const focused = document.activeElement
  return focused !== null && lane.contains(focused) && focused.getAttribute('role') === 'tab' && focused.getAttribute('aria-selected') === 'true'
}
/** M3: alerts past the strip's edge show on the overflow control as glyph + count and are spoken in words. */
function overflowCues(toggle: HTMLElement, beyond: readonly Agent[]): boolean {
  const failed = beyond.filter(agent => agent.status === 'Failed').length
  const waiting = beyond.filter(agent => agent.status === 'Needs you').length
  const cue = (kind: string, count: number) => count === 0 ? toggle.querySelector(`[data-alert-cue="${kind}"]`) === null : toggle.querySelector(`[data-alert-cue="${kind}"]`)?.textContent === String(count) && toggle.querySelector(`[data-alert-cue="${kind}"] svg`) !== null
  return cue('failed', failed) && cue('needs-you', waiting) && (toggle.textContent ?? '').endsWith(alertWords(beyond).map(words => `, ${words}`).join(''))
}
const currentLane = (canvasElement: HTMLElement) => canvasElement.querySelector<HTMLElement>('[data-switching-lane]')!
const nextFrame = () => new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
export const SubagentSwitchingProbe: Story = { args: { probe: true, state: 'many', width: 900 }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  // D1: the first count press after load lands on the selected tab by the next frame; no polling.
  await userEvent.click(within(currentLane(canvasElement)).getByRole('button', { name: activeCountName(workspaces.running) }))
  await nextFrame()
  await expect(currentLane(canvasElement)).toHaveAttribute('data-state', 'running')
  await expect(selectedTabFocused(currentLane(canvasElement))).toBe(true)
  for (const switcher of variants) {
    await userEvent.click(canvas.getByRole('button', { name: switcher }))
    await waitFor(() => expect(currentLane(canvasElement)).toHaveAttribute('data-variant', switcher))
    for (const placement of ['P1', 'P2']) {
      await userEvent.click(canvas.getByRole('button', { name: placement }))
      for (const fixture of sidebarFixtures) {
        const row = currentLane(canvasElement).querySelector<HTMLElement>(`[data-count-row="${fixture.state}"]`)!
        await expect(singleLine(row)).toBe(true)
        await expect(row.getBoundingClientRect().width).toBeLessThanOrEqual(240)
        await expect(countMatches(row, fixture.state)).toBe(true)
        const count = row.querySelector<HTMLElement>('[data-active-count], [data-count-unknown]')
        // Pressing the name opens the agent without moving focus to the switcher; pressing the count does both.
        await userEvent.click(count ?? row.querySelector<HTMLElement>('[data-row-name]')!)
        await waitFor(() => {
          const lane = currentLane(canvasElement)
          expect(lane).toHaveAttribute('data-state', fixture.state)
          if (count !== null) expect(lane).toHaveAttribute('data-active', 'root')
          expect(switcherFocused(lane)).toBe(count !== null)
        })
      }
    }
    await userEvent.click(canvas.getByRole('button', { name: 'many' }))
    const lane = currentLane(canvasElement)
    const controls = within(lane)
    await userEvent.click(controls.getByRole('textbox', { name: 'Message' }))
    await userEvent.keyboard('{Control>}{ArrowRight}{/Control}')
    await waitFor(() => expect(lane).toHaveAttribute('data-active', 'child-1'))
    await expect(lane).toHaveTextContent('Check layout behaviour')
    await userEvent.click(controls.getByRole('textbox', { name: 'Message' }))
    await userEvent.keyboard('{Control>}{ArrowLeft}{/Control}')
    await waitFor(() => expect(lane).toHaveAttribute('data-active', 'root'))
    await userEvent.click(lane.querySelector<HTMLElement>('[data-thread-drawer-toggle]')!)
    const drawer = await controls.findByRole('complementary', { name: 'All agent threads' })
    await expect(drawer.querySelectorAll('[data-thread-item]')).toHaveLength(workspaces.many.agents.length)
    await userEvent.click(within(drawer).getByRole('button', { name: 'Recovery, running' }))
    await waitFor(() => expect(lane).toHaveAttribute('data-active', 'child-14'))
    await expect(lane).toHaveTextContent('Check recovery behaviour')
    await userEvent.click(controls.getByRole('button', { name: 'Close agent threads' }))
    await waitFor(() => expect(lane.querySelector('[data-thread-drawer]')).toBeNull())
    await userEvent.click(controls.getByRole('textbox', { name: 'Message' }))
    await userEvent.keyboard('{Control>}{ArrowRight}{/Control}')
    await waitFor(() => expect(lane).toHaveAttribute('data-active', 'root'))
  }
  // Negative controls: each assertion must fail on a broken row, then pass again once restored.
  let lane = currentLane(canvasElement)
  const row = lane.querySelector<HTMLElement>('[data-count-row="running"]')!
  const name = row.querySelector<HTMLElement>('[data-row-name]')!
  const originalName = name.textContent
  name.append(document.createElement('br'), document.createTextNode('Second line'))
  await expect(singleLine(row)).toBe(false)
  name.textContent = originalName
  await expect(singleLine(row)).toBe(true)
  const countText = row.querySelector<HTMLElement>('[data-count-text]')!
  const originalCount = countText.textContent
  countText.textContent = '99'
  await expect(countMatches(row, 'running')).toBe(false)
  countText.textContent = originalCount
  await expect(countMatches(row, 'running')).toBe(true)
  const spoken = row.querySelector<HTMLElement>('[data-active-count] > span:last-child')!
  const originalSpoken = spoken.textContent
  spoken.textContent = ''
  await expect(countMatches(row, 'running')).toBe(false)
  spoken.textContent = originalSpoken
  await expect(countMatches(row, 'running')).toBe(true)
  await expect(countMatches(lane.querySelector<HTMLElement>('[data-count-row="zero"]')!, 'running')).toBe(false)
  const inert = document.createElement('button')
  inert.textContent = 'Inert count control'
  row.append(inert)
  await userEvent.click(inert)
  await expect(switcherFocused(lane)).toBe(false)
  inert.remove()
  await userEvent.click(row.querySelector<HTMLElement>('[data-active-count]')!)
  await waitFor(() => expect(switcherFocused(currentLane(canvasElement))).toBe(true))
  await userEvent.click(canvas.getByRole('button', { name: 'nested' }))
  lane = currentLane(canvasElement)
  await expect(lane).toHaveAttribute('data-active', 'great-grandchild')
  const ancestry = within(lane).getByRole('navigation', { name: 'Agent ancestry' })
  await expect([...ancestry.querySelectorAll('li')].map(item => item.textContent)).toEqual(['Key map review', 'Layout', 'Key map', 'Shortcut audit, finished'])
  await expect(ancestry.querySelector('[aria-current="page"]')).toHaveTextContent('Shortcut audit')
  await userEvent.click(within(ancestry).getByRole('button', { name: 'Key map review' }))
  await waitFor(() => expect(lane).toHaveAttribute('data-active', 'root'))
  await userEvent.click(canvas.getByRole('button', { name: 'attention' }))
  lane = currentLane(canvasElement)
  await waitFor(() => expect(lane).toHaveAttribute('data-active', 'child-1'))
  await expect(lane.querySelector('[data-needs-you]')).toHaveTextContent('Layout is waiting on your preference')
  await userEvent.type(within(lane).getByRole('textbox', { name: 'Message' }), 'Keep finished agents available.')
  await userEvent.click(within(lane).getByRole('button', { name: 'Send' }))
  await waitFor(() => expect(lane).toHaveTextContent('Keep finished agents available.'))
  // H1: an unreported harness never shows 0. Its count opens the seat, focuses the switcher and explains the gap inline: no empty list, no spinner.
  await userEvent.click(canvas.getByRole('button', { name: 'S2' }))
  await userEvent.click(canvas.getByRole('button', { name: 'P1' }))
  lane = currentLane(canvasElement)
  const unknownRow = lane.querySelector<HTMLElement>('[data-count-row="unknown"]')!
  await expect(countMatches(unknownRow, 'unknown')).toBe(true)
  const unknownMark = unknownRow.querySelector<HTMLElement>('[data-count-text]')!
  unknownMark.textContent = '0'
  await expect(countMatches(unknownRow, 'unknown')).toBe(false)
  unknownMark.textContent = '? subagents'
  await expect(countMatches(unknownRow, 'unknown')).toBe(true)
  await userEvent.click(within(unknownRow).getByRole('button', { name: activeCountName(workspaces.unknown) }))
  await waitFor(() => {
    const opened = currentLane(canvasElement)
    expect(opened).toHaveAttribute('data-state', 'unknown')
    expect(selectedTabFocused(opened)).toBe(true)
  })
  lane = currentLane(canvasElement)
  const explanation = lane.querySelector('[data-switcher] [data-subagents-unreported]')
  await expect(explanation).toHaveTextContent(unreportedExplanation(workspaces.unknown))
  await expect(lane.querySelector('[data-switcher]')).not.toHaveTextContent(/no subagents/i)
  await expect(within(lane).getAllByRole('tab')).toHaveLength(1)
  // M1: every count press lands on the selected tab, including a press for the seat already open, and the next arrow key acts at once.
  await userEvent.click(canvas.getByRole('button', { name: 'many' }))
  lane = currentLane(canvasElement)
  const manyCount = () => within(currentLane(canvasElement)).getByRole('button', { name: activeCountName(workspaces.many) })
  await userEvent.click(manyCount())
  await nextFrame()
  await expect(selectedTabFocused(currentLane(canvasElement))).toBe(true)
  await userEvent.keyboard('{ArrowRight}')
  await waitFor(() => expect(currentLane(canvasElement)).toHaveAttribute('data-active', 'child-1'))
  await expect(selectedTabFocused(currentLane(canvasElement))).toBe(true)
  manyCount().focus()
  await userEvent.keyboard('{Enter}')
  await waitFor(() => {
    const again = currentLane(canvasElement)
    expect(again).toHaveAttribute('data-active', 'root')
    expect(selectedTabFocused(again)).toBe(true)
  })
  lane = currentLane(canvasElement)
  lane.querySelector<HTMLElement>('[data-thread-drawer-toggle]')!.focus()
  await expect(selectedTabFocused(lane)).toBe(false)
  await expect(switcherFocused(lane)).toBe(true)
  // M3: 14 children with one failed and one needs-you past the strip's edge; the overflow control and the drawer keep both cues.
  await userEvent.click(canvas.getByRole('button', { name: 'alerts' }))
  lane = currentLane(canvasElement)
  await expect(lane.querySelectorAll('[role="tab"]')).toHaveLength(3)
  const beyond = workspaces.alerts.agents.slice(3)
  await expect(beyond.map(agent => agent.status)).toContain('Failed')
  await expect(beyond.map(agent => agent.status)).toContain('Needs you')
  const overflow = lane.querySelector<HTMLElement>('[data-thread-drawer-toggle]')!
  await expect(overflow.textContent).toContain(`+${beyond.length} more`)
  await expect(overflowCues(overflow, beyond)).toBe(true)
  const failedCue = overflow.querySelector<HTMLElement>('[data-alert-cue="failed"]')!
  failedCue.remove()
  await expect(overflowCues(overflow, beyond)).toBe(false)
  overflow.querySelector('[data-alert-cue="needs-you"]')!.before(failedCue)
  await expect(overflowCues(overflow, beyond)).toBe(true)
  await userEvent.click(overflow)
  const alertDrawer = await within(lane).findByRole('complementary', { name: 'All agent threads' })
  await expect(within(alertDrawer).getByRole('button', { name: 'Delivery, Failed' }).querySelector('[data-status-word]')).toHaveTextContent('Failed')
  await expect(within(alertDrawer).getByRole('button', { name: 'Recovery, Needs you, 1 unread' }).querySelector('[data-status-word]')).toHaveTextContent('Needs you')
  await expect(within(alertDrawer).queryByRole('button', { name: 'Delivery, running' })).toBeNull()
} }
const styles = stylex.create({
  root: { minHeight: '100vh', boxSizing: 'border-box', padding: s.section, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  title: { margin: s.zero, marginBottom: s.md, fontSize: t.headingSize, lineHeight: t.headingLeading, fontWeight: t.weightMedium },
  lede: { margin: s.zero, maxWidth: g.lane, color: ink.fgSoft },
  pickers: { display: 'flex', flexDirection: 'column', gap: s.xl, marginBlock: s.section },
  picker: { display: 'flex', flexDirection: 'column', gap: s.md },
  pickerLabel: { color: ink.fg, fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightMedium },
  pickerLine: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.md },
  description: { marginInlineStart: s.md, color: ink.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading },
  view: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.lg, marginBottom: s.section, paddingTop: s.xl, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  segment: { display: 'flex', gap: s.md },
  button: { minHeight: g.controlLg, paddingInline: s.lg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  selected: { backgroundColor: surface.rowActive, borderColor: ink.fgMuted, color: ink.fg, ':hover': { backgroundColor: surface.rowActive } },
  matrix: { display: 'flex', flexDirection: 'column', gap: s.section, overflowX: 'auto' },
  previewRow: { display: 'flex', alignItems: 'flex-start', gap: s.section },
  fitNote: { margin: s.zero, marginBottom: s.md, color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  zoom: (scale: string) => ({ zoom: scale }),
})

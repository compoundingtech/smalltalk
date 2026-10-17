import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Button } from 'react-aria-components'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Transcript } from './assistant-ui/composition/Transcript'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import type { WorkLogTurn } from './assistant-ui/taste/work-log'
import type { TextItem } from './assistant-ui/embrace-data/model'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const at = '2026-01-15T12:00:00Z'
const now = Date.parse(at) + 12000
const answer: TextItem & { role: 'assistant' } = { _tag: 'Text', id: 'critique/answer', role: 'assistant', attachments: [], at, streaming: false, text: Array.from({ length: 30 }, (_, index) => `Paragraph ${index + 1}: Keep the reading lane stable while reviewing the selected resource.`).join('\n\n') }
const next: TextItem & { role: 'assistant' } = { ...answer, id: 'critique/next', text: 'The selection and keyboard focus are preserved.' }
const work: WorkLogTurn = { running: true, failed: false, interrupted: false, durationMs: undefined, calls: [
  { id: 'critique/read', kind: 'read', title: 'Read selected resource', status: 'success', startedAt: at, endedAt: '2026-01-15T12:00:02Z', detail: 'The selected resource has a stable identity.' },
  { id: 'critique/run', kind: 'run', title: 'Check a longer command label with complete arguments', argsSummary: 'printf ready', status: 'success', startedAt: at, endedAt: '2026-01-15T12:00:12Z', detail: 'ready' },
  { id: 'critique/cancelled', kind: 'read', title: 'Compare every retained workspace snapshot', status: 'interrupted', startedAt: at, endedAt: '2026-01-15T12:59:59Z' },
  { id: 'critique/failed', kind: 'edit', title: 'Apply the reviewed migration to every retained workspace snapshot and verify each resulting identity before release', status: 'error', startedAt: at, endedAt: '2026-01-16T11:59:00Z', detail: 'The retained workspace snapshot changed during review.' },
  { id: 'critique/wait', kind: 'read', title: 'Await the next observation', status: 'running', startedAt: at },
] }
const hint = 'Messages can be sent once this conversation loads.'
const draft = 'Keep this retained draft'
const failures = ['none', 'answer-meta', 'cold-composer', 'cold-slab', 'retained-reason', 'tool-rows', 'tool-overflow', 'run-gutter', 'thinking', 'thinking-ink', 'row-fade'] as const
/** WCAG contrast of a computed `rgb()`/`rgba()` ink over an opaque computed background, alpha composited first. */
const contrast = (fg: string, bg: string) => {
  const [br, bgreen, bb] = bg.match(/[\d.]+/g)!.map(Number)
  const [r, green, b, a = 1] = fg.match(/[\d.]+/g)!.map(Number)
  const luminance = (channels: readonly number[]) => channels.map(value => value / 255).map(value => value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4).reduce((sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index]!, 0)
  const text = luminance([r!, green!, b!].map((value, index) => a * value + (1 - a) * [br!, bgreen!, bb!][index]!)), paper = luminance([br!, bgreen!, bb!])
  return (Math.max(text, paper) + 0.05) / (Math.min(text, paper) + 0.05)
}
type Failure = typeof failures[number]
const styles = stylex.create({
  root: { width: g.polishWide, maxWidth: '100%', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  narrow: { width: g.polishNarrow },
  transcript: { height: g.transcriptFrameHeight },
  controls: { display: 'flex', flexWrap: 'wrap', gap: s.md, paddingBlock: s.lg },
})

function Critique({ scheme = 'dark', narrow = false, failure = 'none' }: { scheme?: Scheme; narrow?: boolean; failure?: Failure }) {
  const root = React.useRef<HTMLElement>(null)
  const [revision, rerender] = React.useState(0)
  const [locked, setLocked] = React.useState(false)
  // Deliberately restore one old behavior after the production tree re-renders.
  // Each selectable negative control must break its corresponding measured assertion.
  React.useLayoutEffect(() => {
    if (revision === 0 || root.current === null) return
    const element = <T extends HTMLElement>(selector: string) => root.current!.querySelector<T>(selector)!
    if (failure === 'answer-meta') element('[data-testid="message-header"]').style.order = '1'
    if (failure === 'cold-composer') element('[aria-label="Cold composer example"] [data-testid="composer-placeholder"]').textContent = 'Message'
    if (failure === 'cold-slab') element('[aria-label="Cold slab composer example"] form').style.flexDirection = 'column'
    if (failure === 'retained-reason' && locked) { const reason = element('[aria-label="Retained draft example"] [data-testid="composer-reason"]'); reason.style.display = 'none'; element('[aria-label="Retained draft example"] textarea').removeAttribute('aria-describedby') }
    if (failure === 'tool-rows') element('[data-tool-status="success"] [data-call-duration]').textContent = 'OK · 2s'
    if (failure === 'tool-overflow') element('[data-tool-status="interrupted"] [data-call-word]').textContent = 'Interrupted after the retained cancellation request'
    // The shipped R4 regression: the run state sat in the 80px end column, past the disclosure, in the gutter.
    if (failure === 'run-gutter') { element('[data-tool-status="running"] [data-call-word]').textContent = ''; element('[data-tool-status="running"] [data-call-duration]').textContent = 'Running' }
    if (failure === 'thinking') element('[data-testid="thinking-body"]').style.marginInlineStart = '0'
    // The opposite scheme's muted ink: the dark value in light (the shipped 3.79:1 bug), the light value in dark.
    if (failure === 'thinking-ink') element('[data-testid="thinking-body"] p').style.color = scheme === 'light' ? '#818181' : '#66666e'
    if (failure === 'row-fade') element('[data-tool-status="success"] > button').style.transitionDuration = '0s'
  }, [failure, revision, locked, scheme])
  return <main ref={root} data-revision={revision} {...stylex.props(styles.root, narrow && styles.narrow, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.controls)}><Button onPress={() => rerender(value => value + 1)}>Apply selected regression</Button><Button onPress={() => setLocked(true)}>Lock retained draft</Button><span>{failure}</span></div>
    <section aria-label="Answer example" {...stylex.props(styles.transcript)}><EmbraceRuntimeProvider options={{ messages: [answer, next], onNew: async () => {} }}><Transcript title="Review answers" now={now} observedAt={now} sync={{ _tag: 'Live', since: now }} turns={[{ id: 'critique', items: [answer, next], work: { ...work, calls: [], running: false } }]} /></EmbraceRuntimeProvider></section>
    <section aria-label="Tool row example"><WorkLogV1 turn={work} hideLiveRow /></section>
    <section aria-label="Thinking example"><ThinkingEntry text="Compare **reported rows** with the input, then review the retained selection." /></section>
    <section aria-label="Cold composer example"><EmbraceRuntimeProvider options={{ messages: [], isDisabled: true, onNew: async () => {} }}><EmbraceComposer variant="C1" disabledReason={hint} /></EmbraceRuntimeProvider></section>
    <section aria-label="Cold slab composer example"><EmbraceRuntimeProvider options={{ messages: [], isDisabled: true, onNew: async () => {} }}><EmbraceComposer variant="C2" disabledReason={hint} /></EmbraceRuntimeProvider></section>
    <section aria-label="Retained draft example"><EmbraceRuntimeProvider options={{ messages: [], onNew: async () => {} }}><EmbraceComposer variant="C1" disabledReason={locked ? hint : undefined} /></EmbraceRuntimeProvider></section>
  </main>
}
const meta = {
  title: 'Fractal UI/Live critique r2', component: Critique, parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', narrow: false, failure: 'none' },
  argTypes: { scheme: { control: 'radio', options: ['light', 'dark'] }, narrow: { control: 'boolean' }, failure: { control: 'select', options: failures, description: 'Select one old behavior. Its production-component play must fail after Apply selected regression re-renders the tree.' } },
} satisfies Meta<typeof Critique>
export default meta
type Story = StoryObj<typeof meta>

export const Dark1440: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findAllByTestId('agent-message')
  const thinking = await canvas.findByRole('button', { name: 'Thinking' })
  await userEvent.click(thinking)
  await expect(thinking).toHaveAttribute('aria-expanded', 'true')
  const retained = canvas.getByRole('region', { name: 'Retained draft example' })
  const retainedField = within(retained).getByRole('textbox')
  await userEvent.type(retainedField, draft)
  await expect(retainedField).toHaveValue(draft)
  const apply = canvas.getByRole('button', { name: 'Apply selected regression' })
  await userEvent.click(apply)
  await waitFor(() => expect(canvasElement.querySelector('main')).toHaveAttribute('data-revision', '1'))
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches

  // 4: Copy and time live in the 16px header line above the body; there is no footer row and no extra Tab stop.
  const answers = await canvas.findAllByTestId('agent-message')
  await expect(answers[0]).not.toHaveAttribute('tabindex')
  const header = within(answers[0]!).getByTestId('message-header')
  const copy = within(answers[0]!).getByRole('button', { name: 'Copy answer' })
  const markdown = within(answers[0]!).getByTestId('markdown')
  // Read the exact endpoint only after the native transition finishes; a sampled final easing frame can be nonzero.
  const settleCopy = async () => {
    await new Promise(resolve => requestAnimationFrame(resolve))
    await Promise.all(copy.getAnimations().map(animation => animation.finished))
    await new Promise(resolve => requestAnimationFrame(resolve))
  }
  await expect(header).toContainElement(copy)
  await expect(header).toContainElement(answers[0]!.querySelector('time'))
  await expect(header.getBoundingClientRect().height).toBe(16)
  await expect(header.getBoundingClientRect().bottom).toBeLessThanOrEqual(markdown.getBoundingClientRect().top)
  await expect(answers[0]!.getBoundingClientRect().bottom - markdown.getBoundingClientRect().bottom).toBeLessThanOrEqual(2)
  await expect(answers[1]).toHaveAttribute('data-run-continuation', 'true')
  await expect(within(answers[1]!).queryByTestId('message-header')).toBeNull()
  await expect(getComputedStyle(copy).opacity).toBe('0')
  await expect(getComputedStyle(copy).transitionDuration).toBe(reduced ? '0s' : '0.15s')
  await userEvent.hover(answers[0]!)
  await settleCopy()
  await waitFor(() => expect(getComputedStyle(copy).opacity).toBe('1'))
  await userEvent.unhover(answers[0]!)
  await settleCopy()
  await waitFor(() => expect(getComputedStyle(copy).opacity).toBe('0'))
  apply.focus()
  for (let step = 0; step < 8 && document.activeElement !== copy; step++) await userEvent.tab()
  await expect(copy).toHaveFocus()
  await settleCopy()
  await waitFor(() => expect(getComputedStyle(copy).opacity).toBe('1'))
  apply.focus()
  await settleCopy()
  await waitFor(() => expect(getComputedStyle(copy).opacity).toBe('0'))

  // 9: empty disabled composers keep one compact row with the reason in the placeholder.
  for (const name of ['Cold composer example', 'Cold slab composer example']) {
    const cold = canvas.getByRole('region', { name })
    await expect(within(cold).getByTestId('composer-placeholder')).toHaveTextContent(hint)
    await expect(getComputedStyle(cold.querySelector('form')!).flexDirection).toBe('row')
    await expect(within(cold).getByTestId('kit-composer').getBoundingClientRect().height).toBeLessThanOrEqual(50)
    await expect(within(cold).getByRole('textbox')).toHaveAccessibleDescription(hint)
    await expect(within(cold).getByRole('button', { name: 'Send' })).toBeDisabled()
  }

  // 9: a retained draft that becomes read-only keeps a visible, described reason.
  await userEvent.click(canvas.getByRole('button', { name: 'Lock retained draft' }))
  await waitFor(() => expect(retainedField).toBeDisabled())
  await expect(retainedField).toHaveValue(draft)
  await expect(within(retained).getByTestId('composer-reason')).toHaveTextContent(hint)
  await expect(within(retained).getByTestId('composer-reason').getBoundingClientRect().height).toBeGreaterThan(0)
  await expect(retainedField).toHaveAccessibleDescription(hint)

  // 10, 12: success rows show the duration only; every chevron and duration column is pinned to the row end.
  // R4: a running row says so in its word slot, in flow after the title and before the disclosure; its end column stays empty.
  const tools = canvas.getByRole('region', { name: 'Tool row example' })
  const row = (status: string) => tools.querySelector<HTMLElement>(`[data-tool-status="${status}"]`)!.firstElementChild as HTMLElement
  const rows = Array.from(tools.querySelectorAll<HTMLElement>('[data-tool-status]'), entry => entry.firstElementChild as HTMLElement)
  await expect(rows).toHaveLength(5)
  const success = tools.querySelectorAll<HTMLElement>('[data-tool-status="success"] > button')
  await expect(success).toHaveLength(2)
  await expect(success[0]!).not.toHaveTextContent('OK')
  await expect(success[0]!.querySelector('[data-call-duration]')).toHaveTextContent(/^2s$/)
  await expect(success[1]!.querySelector('[data-call-duration]')).toHaveTextContent(/^12s$/)
  const runningWord = row('running').querySelector<HTMLElement>('[data-call-word]')!
  await expect(runningWord).toHaveTextContent(/^Running$/)
  await expect(row('running').querySelector('[data-call-duration]')).toHaveTextContent(/^$/)
  await expect(runningWord.getBoundingClientRect().left).toBeGreaterThanOrEqual(row('running').querySelector('[data-call-title]')!.getBoundingClientRect().right)
  await expect(runningWord.getBoundingClientRect().right).toBeLessThanOrEqual(row('running').children[3]!.getBoundingClientRect().left)
  await expect(row('interrupted').querySelector('[data-call-word]')).toHaveTextContent(/^Interrupted$/)
  await expect(row('interrupted').querySelector('[data-call-duration]')).toHaveTextContent(/^59m 59s$/)
  await expect(row('error').querySelector('[data-call-word]')).toHaveTextContent(/^Failed$/)
  await expect(row('error').querySelector('[data-call-duration]')).toHaveTextContent(/^23h 59m$/)
  const chevron = rows[0]!.children[3]!.getBoundingClientRect().left
  for (const entry of rows) {
    const box = entry.getBoundingClientRect()
    const duration = entry.querySelector<HTMLElement>('[data-call-duration]')!
    const word = entry.querySelector<HTMLElement>('[data-call-word]')!
    await expect(box.height).toBe(24)
    await expect(Math.abs(entry.children[3]!.getBoundingClientRect().left - chevron)).toBeLessThan(0.5)
    await expect(Math.abs(duration.getBoundingClientRect().right - box.right)).toBeLessThan(0.5)
    await expect(duration.getBoundingClientRect().width).toBe(80)
    await expect(duration.scrollWidth).toBeLessThanOrEqual(duration.clientWidth)
    await expect(word.scrollWidth).toBeLessThanOrEqual(word.clientWidth)
    await expect(entry.scrollWidth).toBeLessThanOrEqual(entry.clientWidth)
  }
  await expect(getComputedStyle(success[0]!).transitionDuration).toBe(reduced ? '0s' : '0.15s')
  await userEvent.click(success[0]!)
  await expect(success[0]!).toHaveAttribute('aria-expanded', 'true')
  // Main's two-stage detail: the expanded row shows its intent; output waits behind the raw disclosure.
  await expect(canvas.queryByTestId('work-call-output')).toBeNull()
  await userEvent.click(within(success[0]!.parentElement!).getByRole('button', { name: 'Show raw input/output' }))
  await expect(canvas.getByTestId('work-call-output')).toHaveTextContent('stable identity')

  // 11: Thinking is a work row; its framed body starts on the text column and renders muted 13px Markdown
  // in this scheme's muted ink, which must clear AA (4.5:1) on the canvas in light and dark.
  await expect(parseFloat(getComputedStyle(thinking).fontSize)).toBe(14)
  await expect(thinking.getBoundingClientRect().height).toBe(24)
  const body = canvas.getByTestId('thinking-body')
  await expect(body.getBoundingClientRect().left - thinking.getBoundingClientRect().left).toBe(22)
  await expect(parseFloat(getComputedStyle(body).borderTopWidth)).toBe(1)
  const paragraph = body.querySelector('p')!
  await expect(parseFloat(getComputedStyle(paragraph).fontSize)).toBe(13)
  await expect(getComputedStyle(paragraph).color).toBe(getComputedStyle(thinking.querySelector('[data-call-title]')!).color)
  await expect(getComputedStyle(body.querySelector('strong')!).color).toBe(getComputedStyle(paragraph).color)
  await expect(contrast(getComputedStyle(paragraph).color, getComputedStyle(canvasElement.querySelector('main')!).backgroundColor)).toBeGreaterThanOrEqual(4.5)
  await expect(getComputedStyle(thinking).transitionDuration).toBe(reduced ? '0s' : '0.15s')
  await userEvent.click(thinking)
  await expect(canvas.queryByTestId('thinking-body')).toBeNull()
} }
export const Light1440: Story = { ...Dark1440, args: { scheme: 'light' } }
export const Dark440: Story = { ...Dark1440, args: { narrow: true } }
export const Light440: Story = { ...Dark1440, args: { narrow: true, scheme: 'light' } }
export const AllStates: Story = { ...Dark1440 }

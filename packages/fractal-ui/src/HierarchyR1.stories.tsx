import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Button } from 'react-aria-components'
import { expect, waitFor, userEvent, within } from 'storybook/test'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import type { WorkLogCall, WorkLogTurn } from './assistant-ui/taste/work-log'
import type { TextItem } from './assistant-ui/embrace-data/model'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const at = '2026-01-15T12:00:00Z'
const now = Date.parse(at) + 90000
const ended = (ms: number) => new Date(Date.parse(at) + ms).toISOString()
// Both non-success titles overflow even the 1440 lane, so the ellipsis is measurable at every width.
const failedTitle = 'Apply the reviewed migration to every retained workspace snapshot, then verify each resulting identity against the signed release manifest, compare the archived review notes, and confirm the summary before it is published to the review channel for the next release window'
const cancelledTitle = 'Read every archived review note for the retained workspace snapshots in order, including the notes attached to each earlier hash, the signed manifest history, and the release summaries that reference any identity reviewed during this release window'
const calls: readonly WorkLogCall[] = [
  { id: 'fold/read-plan', kind: 'read', title: 'Read release plan', argsSummary: 'docs/release-plan.md', status: 'success', startedAt: at, endedAt: ended(400), detail: 'Three checks run in order.' },
  { id: 'fold/list', kind: 'run', title: 'List snapshots', argsSummary: 'ls snapshots', status: 'success', startedAt: at, endedAt: ended(9000), detail: 'alpha\nbeta\ngamma' },
  { id: 'fold/compare', kind: 'run', title: 'Compare identities', argsSummary: 'diff alpha.json beta.json', status: 'success', startedAt: at, endedAt: ended(145000), detail: 'No differences.' },
  { id: 'fold/edit', kind: 'edit', title: 'Edit summary', argsSummary: 'notes/summary.md', status: 'success', startedAt: at, endedAt: ended(3720000), detail: 'Summary updated.' },
  { id: 'fold/failed', kind: 'edit', title: failedTitle, status: 'error', startedAt: at, endedAt: ended(86340000), detail: 'The retained snapshot changed during review.' },
  { id: 'fold/hash', kind: 'run', title: 'Hash manifest', argsSummary: 'sha256sum manifest.json', status: 'success', startedAt: at, endedAt: ended(2000), detail: 'ok manifest.json' },
  { id: 'fold/cancelled', kind: 'read', title: cancelledTitle, status: 'interrupted', startedAt: at, endedAt: ended(3599000) },
  { id: 'fold/scan', kind: 'read', title: 'Search release notes', argsSummary: 'identity', status: 'success', startedAt: at, endedAt: ended(12000), detail: 'Two matches.' },
  { id: 'fold/check', kind: 'run', title: 'Check signatures', argsSummary: 'verify --all', status: 'success', startedAt: at, endedAt: ended(31000), detail: 'All signatures verified.' },
  // Fourteen rows (more than twelve) overflow the fold's height cap, so the shared child scroller is exercised.
  { id: 'fold/tag', kind: 'run', title: 'Tag release', argsSummary: 'git tag --list', status: 'success', startedAt: at, endedAt: ended(1000), detail: 'Tag listed.' },
  { id: 'fold/pack', kind: 'run', title: 'Pack archive', argsSummary: 'tar -czf release.tgz dist', status: 'success', startedAt: at, endedAt: ended(18000), detail: 'Archive written.' },
  { id: 'fold/notes', kind: 'read', title: 'Read changelog', argsSummary: 'CHANGELOG.md', status: 'success', startedAt: at, endedAt: ended(700), detail: 'Four entries.' },
  { id: 'fold/draft', kind: 'edit', title: 'Draft announcement', argsSummary: 'notes/announce.md', status: 'success', startedAt: at, endedAt: ended(5000), detail: 'Draft saved.' },
  { id: 'fold/wait', kind: 'read', title: 'Await the next observation', status: 'running', startedAt: at },
]
const work: WorkLogTurn = { calls, running: true, failed: false, interrupted: false, durationMs: undefined, startedAt: at }
const richText = [
  'The release plan keeps **three checks** in order.',
  '- Confirm the retained snapshot\n  - Compare each identity\n  - Record the reviewed hash\n- Publish the summary',
  '> Hold the release if **any identity** changes during review.',
  '```ts\nexport const releaseGate = (changed: number) => changed === 0\n```',
  '1. Confirm the retained snapshot.\n\n   Record the reviewed hash beside it.\n\n2. Publish the summary.',
].join('\n\n')
const assistant = (id: string, text: string): TextItem & { role: 'assistant' } => ({ _tag: 'Text', id, role: 'assistant', attachments: [], at, streaming: false, text })
const rich = assistant('hierarchy/rich', richText)
const run = [
  assistant('hierarchy/run-1', 'The snapshot identities match the signed manifest.'),
  assistant('hierarchy/run-2', 'Two archived notes mention the earlier hash; neither changes the outcome.'),
  assistant('hierarchy/run-3', 'The summary is ready for review.'),
]
const idle: WorkLogTurn = { calls: [], running: false, failed: false, interrupted: false, durationMs: undefined }
const caption = 'Release reviewer'
const turns = (splitRun: boolean, runShown: boolean): readonly TranscriptTurn[] => [
  { id: 'hierarchy/rich-turn', items: [rich], work: idle, senderCaptions: { [rich.id]: caption } },
  // The split restores a header Copy that only covers the first block: later blocks lose the shared sender.
  ...runShown ? [{ id: 'hierarchy/run-turn', items: run, work: idle, senderCaptions: splitRun ? { [run[0]!.id]: caption } : Object.fromEntries(run.map(item => [item.id, caption])) }] : [],
]
const failures = ['none', 'fold-indent', 'rail', 'chevron-x', 'status-end', 'word-clip', 'title-wrap', 'thinking-row', 'fold-scroll', 'answer-footer', 'markdown-margin', 'loose-list', 'quote-strong', 'run-copy', 'copy-reject', 'copy-late', 'prose-run', 'seat-header'] as const
type Failure = typeof failures[number]
const styles = stylex.create({
  root: { width: g.polishWide, maxWidth: '100%', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  narrow: { width: g.polishNarrow },
  transcript: { height: g.transcriptFrameHeight },
  controls: { display: 'flex', flexWrap: 'wrap', gap: s.md, paddingBlock: s.lg },
})

function Hierarchy({ scheme = 'dark', narrow = false, failure = 'none', showSeatHeader = true }: { scheme?: Scheme; narrow?: boolean; failure?: Failure; showSeatHeader?: boolean }) {
  const root = React.useRef<HTMLElement>(null)
  const [revision, rerender] = React.useState(0)
  const [runShown, setRunShown] = React.useState(true)
  const [remounts, setRemounts] = React.useState(0)
  // Each selectable negative control restores one old behavior after the production tree re-renders; its measured assertion must fail.
  React.useLayoutEffect(() => {
    if (revision === 0 || root.current === null) return
    const element = <T extends HTMLElement>(selector: string) => root.current!.querySelector<T>(selector)!
    if (failure === 'fold-indent') element('[data-testid="work-log-children"]').style.paddingInlineStart = '0'
    if (failure === 'rail') { const style = document.createElement('style'); style.textContent = '[data-testid="work-log-children"]::before { display: none }'; root.current.append(style) }
    if (failure === 'chevron-x') element('[data-tool-status="error"] [data-call-word]').style.display = 'none'
    if (failure === 'status-end') element('[data-tool-status="success"] [data-call-duration]').style.textAlign = 'left'
    if (failure === 'word-clip') element('[data-tool-status="interrupted"] [data-call-word]').style.maxWidth = '2ch'
    if (failure === 'title-wrap') element('[data-tool-status="error"] [data-call-title]').style.whiteSpace = 'normal'
    if (failure === 'thinking-row') element('[data-testid="thinking-entry"] button').style.gridTemplateColumns = 'auto auto'
    if (failure === 'fold-scroll') element('[data-testid="work-log-children"]').append(element('[data-testid="thinking-entry"]'))
    if (failure === 'answer-footer') element('[data-item-id="hierarchy/rich"] [data-testid="message-header"]').style.order = '1'
    if (failure === 'markdown-margin') (element('[data-item-id="hierarchy/rich"] [data-testid="markdown"]').lastElementChild as HTMLElement).style.marginBlockEnd = '10px'
    if (failure === 'loose-list') element('[data-item-id="hierarchy/rich"] ol > li > p + p').style.marginBlockStart = '0'
    // Quoted strong text lifts to the unquoted strong ink, as it did before quotes set the muted tone.
    if (failure === 'quote-strong') element('[data-item-id="hierarchy/rich"] blockquote strong').style.color = getComputedStyle(element('[data-item-id="hierarchy/rich"] p > strong')).color
    if (failure === 'copy-reject') element('[data-item-id="hierarchy/run-1"] [aria-label="Copy answer"]').dataset.copied = 'true'
    // A write that settles after unmount marks the remounted run copied, as an unguarded success handler would.
    if (failure === 'copy-late' && remounts > 0) element('[data-item-id="hierarchy/run-1"] [aria-label="Copy answer"]').dataset.copied = 'true'
    if (failure === 'prose-run') { const header = element('[data-item-id="hierarchy/run-1"] [data-testid="message-header"]'); element('[data-item-id="hierarchy/run-2"]').prepend(header.cloneNode(true)) }
  }, [failure, revision, remounts])
  return <main ref={root} data-revision={revision} {...stylex.props(styles.root, narrow && styles.narrow, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.controls)}><Button onPress={() => rerender(value => value + 1)}>Apply selected regression</Button><Button onPress={() => setRunShown(false)}>Hide run</Button><Button onPress={() => { setRunShown(true); setRemounts(value => value + 1) }}>Show run</Button><span>{failure}</span></div>
    <section aria-label="Fold example"><WorkLogV1 turn={work} hideLiveRow ariaLabel="Release work" expandedBody={<ThinkingEntry text="Check the **retained snapshot** first, then compare identities." />} /></section>
    <section aria-label="Message example" {...stylex.props(styles.transcript)}><EmbraceRuntimeProvider options={{ messages: [rich, ...run], onNew: async () => {} }}><Transcript title="Release review" now={now} observedAt={now} sync={{ _tag: 'Live', since: now }} showSeatHeader={failure === 'seat-header' && revision > 0 ? !showSeatHeader : showSeatHeader} turns={turns(failure === 'run-copy' && revision > 0, runShown)} /></EmbraceRuntimeProvider></section>
  </main>
}
const meta = {
  title: 'Fractal UI/Hierarchy r1', component: Hierarchy, parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', narrow: false, failure: 'none' },
  argTypes: { scheme: { control: 'radio', options: ['light', 'dark'] }, narrow: { control: 'boolean' }, failure: { control: 'select', options: failures, description: 'Select one old behavior. Its production-component play must fail after Apply selected regression re-renders the tree.' } },
} satisfies Meta<typeof Hierarchy>
export default meta
type Story = StoryObj<typeof meta>

export const Dark1440: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findAllByTestId('agent-message')
  const thinking = canvas.getByRole('button', { name: 'Thinking' })
  await userEvent.click(thinking)
  await expect(thinking).toHaveAttribute('aria-expanded', 'true')
  await userEvent.click(canvas.getByRole('button', { name: 'Apply selected regression' }))
  await waitFor(() => expect(canvasElement.querySelector('main')).toHaveAttribute('data-revision', '1'))

  // Fold: children start one text column (22) in, beside a 1px rail on the header chevron's centre (7.5).
  const fold = canvas.getByRole('region', { name: 'Release work' })
  const lane = fold.getBoundingClientRect()
  const children = within(fold).getByTestId('work-log-children')
  const rail = getComputedStyle(children, '::before')
  const rows = Array.from(children.querySelectorAll<HTMLElement>('[data-tool-status]'), entry => entry.firstElementChild as HTMLElement)
  const thinkingRow = within(children).getByTestId('thinking-entry').querySelector('button')!
  await expect(rows).toHaveLength(14)
  await expect(fold.querySelector('hr')).toBeNull()
  for (const entry of [...rows, thinkingRow]) await expect(entry.getBoundingClientRect().left - lane.left).toBeCloseTo(22, 1)
  await expect(rail.display).toBe('block')
  await expect(rail.left).toBe('7.5px')
  await expect(rail.width).toBe('1px')
  await expect(rail.bottom).toBe('4px')

  // Scrolling fold: tool rows and Thinking share one scroller, so a classic scrollbar narrows them alike
  // and every row ends on the scroller's content edge.
  const scroller = rows[0]!.parentElement!.parentElement!
  await expect(scroller.scrollHeight).toBeGreaterThan(scroller.clientHeight)
  await expect(scroller).toContainElement(thinkingRow)
  const edge = scroller.getBoundingClientRect().left + scroller.clientLeft + scroller.clientWidth
  for (const entry of [...rows, thinkingRow]) await expect(Math.abs(entry.getBoundingClientRect().right - edge)).toBeLessThan(0.5)

  // Rows: every chevron shares one x and every duration ends at the lane edge, including the longest values.
  const chevron = rows[0]!.children[3]!.getBoundingClientRect().left
  for (const entry of [...rows, thinkingRow]) {
    await expect(entry.getBoundingClientRect().height).toBe(24)
    await expect(Math.abs(entry.children[3]!.getBoundingClientRect().left - chevron)).toBeLessThan(0.5)
  }
  await expect(Math.abs(chevron - (edge - 80 - 6 - 16))).toBeLessThan(0.5)
  for (const entry of rows) {
    const duration = entry.querySelector<HTMLElement>('[data-call-duration]')!
    await expect(getComputedStyle(duration).fontVariantNumeric).toBe('tabular-nums')
    const text = document.createRange()
    text.selectNodeContents(duration)
    await expect(Math.abs(text.getBoundingClientRect().right - edge)).toBeLessThan(1)
    await expect(duration.scrollWidth).toBeLessThanOrEqual(duration.clientWidth)
  }
  const status = (state: string) => children.querySelector<HTMLElement>(`[data-tool-status="${state}"]`)!.firstElementChild as HTMLElement
  await expect(status('error').querySelector('[data-call-duration]')).toHaveTextContent(/^23h 59m$/)
  await expect(status('interrupted').querySelector('[data-call-duration]')).toHaveTextContent(/^59m 59s$/)

  // Non-success rows keep their state word visible beside the chevron; the title ellipsizes before it.
  for (const state of ['error', 'interrupted']) {
    const entry = status(state)
    const word = entry.querySelector<HTMLElement>('[data-call-word]')!
    const title = entry.querySelector<HTMLElement>('[data-call-title]')!
    await expect(word).toHaveTextContent(state === 'error' ? /^Failed$/ : /^Interrupted$/)
    await expect(word.getBoundingClientRect().width).toBeGreaterThan(0)
    await expect(word.scrollWidth).toBeLessThanOrEqual(word.clientWidth)
    await expect(word.getBoundingClientRect().right).toBeLessThanOrEqual(entry.children[3]!.getBoundingClientRect().left)
    await expect(title.getBoundingClientRect().right).toBeLessThanOrEqual(word.getBoundingClientRect().left)
    await expect(getComputedStyle(title).textOverflow).toBe('ellipsis')
    await expect(title.scrollWidth).toBeGreaterThan(title.clientWidth)
    await expect(title.getBoundingClientRect().height).toBe(20)
  }
  await expect(status('error')).toHaveAccessibleName(/Failed/)

  // Thinking matches a tool row: height, type, columns and title column.
  const toolTitle = rows[0]!.querySelector('[data-call-title]')!
  const thinkingTitle = thinkingRow.querySelector('[data-call-title]')!
  await expect(getComputedStyle(thinkingRow).gridTemplateColumns).toBe(getComputedStyle(rows[0]!).gridTemplateColumns)
  await expect(getComputedStyle(thinkingTitle).fontSize).toBe(getComputedStyle(toolTitle).fontSize)
  await expect(getComputedStyle(thinkingTitle).lineHeight).toBe(getComputedStyle(toolTitle).lineHeight)
  await expect(thinkingTitle.getBoundingClientRect().left).toBeCloseTo(toolTitle.getBoundingClientRect().left, 1)
  await expect(canvas.getByTestId('thinking-body').getBoundingClientRect().left - thinkingRow.getBoundingClientRect().left).toBeCloseTo(22, 1)

  // Message: header (12/16) holds time and Copy, the body follows; no footer row.
  const answer = canvasElement.querySelector<HTMLElement>('[data-item-id="hierarchy/rich"]')!
  const header = within(answer).getByTestId('message-header')
  const markdown = within(answer).getByTestId('markdown')
  await expect(header.getBoundingClientRect().height).toBe(16)
  await expect(getComputedStyle(header).fontSize).toBe('12px')
  await expect(header).toContainElement(answer.querySelector('time'))
  await expect(header).toContainElement(within(answer).getByRole('button', { name: 'Copy answer' }))
  await expect(header.getBoundingClientRect().bottom).toBeLessThanOrEqual(markdown.getBoundingClientRect().top)
  await expect(answer.getBoundingClientRect().bottom - markdown.getBoundingClientRect().bottom).toBeLessThanOrEqual(2)

  // Markdown rhythm: 8 between siblings, no outer margin, nested list rail and open marker, quote rail, compact code.
  const blocks = Array.from(markdown.children) as HTMLElement[]
  await expect(blocks).toHaveLength(5)
  for (const block of blocks) {
    await expect(getComputedStyle(block).marginBlockStart).toBe('0px')
    await expect(getComputedStyle(block).marginBlockEnd).toBe('0px')
  }
  for (let index = 1; index < blocks.length; index++) await expect(blocks[index]!.getBoundingClientRect().top - blocks[index - 1]!.getBoundingClientRect().bottom).toBeCloseTo(8, 1)
  await expect(markdown.getBoundingClientRect().bottom).toBeCloseTo(blocks.at(-1)!.getBoundingClientRect().bottom, 1)
  const nested = markdown.querySelector<HTMLElement>('[data-list-depth="1"]')!
  await expect(getComputedStyle(blocks[1]!).paddingInlineStart).toBe('22px')
  await expect(getComputedStyle(nested).borderInlineStartWidth).toBe('1px')
  await expect(getComputedStyle(nested).listStyleType).toBe('circle')
  const quote = markdown.querySelector('blockquote')!
  await expect(getComputedStyle(quote).borderInlineStartWidth).toBe('2px')
  await expect(getComputedStyle(quote).paddingInlineStart).toBe('12px')
  await expect(getComputedStyle(quote.querySelector('p')!).color).toBe(getComputedStyle(header).color)
  await expect(getComputedStyle(quote.querySelector('strong')!).color).toBe(getComputedStyle(quote.querySelector('p')!).color)
  await expect(getComputedStyle(quote.querySelector('strong')!).color).not.toBe(getComputedStyle(blocks[0]!.querySelector('strong')!).color)
  const code = within(markdown).getByTestId('markdown-code-block')
  await expect(getComputedStyle(code).borderTopLeftRadius).toBe('6px')
  await expect(getComputedStyle(code.querySelector('code')!).fontSize).toBe('12px')
  await expect(getComputedStyle(code.querySelector('code')!).lineHeight).toBe('18px')

  // Loose list: 8 between blocks inside an item, 2 between items, nothing after the last block.
  const loose = markdown.querySelector<HTMLElement>('ol')!
  const items = Array.from(loose.children) as HTMLElement[]
  const [lead, follow] = Array.from(items[0]!.querySelectorAll('p'))
  await expect(follow).toBeDefined()
  await expect(follow!.getBoundingClientRect().top - lead!.getBoundingClientRect().bottom).toBeCloseTo(8, 1)
  await expect(lead!.getBoundingClientRect().top).toBeCloseTo(items[0]!.getBoundingClientRect().top, 1)
  await expect(items[1]!.getBoundingClientRect().top - items[0]!.getBoundingClientRect().bottom).toBeCloseTo(2, 1)
  await expect(loose.getBoundingClientRect().bottom).toBeCloseTo(items[1]!.querySelector('p')!.getBoundingClientRect().bottom, 1)

  // Prose run: the single header Copy takes every block of the run, joined by a blank line. A rejected
  // write is handled (no unhandled rejection, no copied state); a write that settles after the run
  // unmounts leaves the remounted run uncopied; a successful one sets data-copied.
  const runBlock = (index: number) => canvasElement.querySelector<HTMLElement>(`[data-item-id="${run[index]!.id}"]`)
  let runCopy = within(runBlock(0)!).getByRole('button', { name: 'Copy answer' })
  const clipboard = navigator.clipboard
  const original = Object.getOwnPropertyDescriptor(clipboard, 'writeText')
  const unhandled: unknown[] = []
  const record = (event: PromiseRejectionEvent) => unhandled.push(event.reason)
  let copied = '', attempts = 0
  window.addEventListener('unhandledrejection', record)
  try {
    Object.defineProperty(clipboard, 'writeText', { configurable: true, value: async () => { attempts++; throw new DOMException('Clipboard permission denied', 'NotAllowedError') } })
    await userEvent.click(runCopy)
    await waitFor(() => expect(attempts).toBe(1))
    // The package lib is ES2022 (no Promise.withResolvers). Two frames let a stray rejection surface.
    for (let frame = 0; frame < 2; frame++) await new Promise(resolve => requestAnimationFrame(resolve))
    await expect(unhandled).toEqual([])
    await expect(runCopy).not.toHaveAttribute('data-copied')
    // The package lib is ES2022 (no Promise.withResolvers): keep the deferred write's resolver.
    let release = () => {}
    Object.defineProperty(clipboard, 'writeText', { configurable: true, value: () => new Promise<void>(resolve => { release = resolve }) })
    await userEvent.click(runCopy)
    await userEvent.click(canvas.getByRole('button', { name: 'Hide run' }))
    await waitFor(() => expect(runBlock(0)).toBeNull())
    release()
    for (let frame = 0; frame < 2; frame++) await new Promise(resolve => requestAnimationFrame(resolve))
    await userEvent.click(canvas.getByRole('button', { name: 'Show run' }))
    runCopy = await waitFor(() => within(runBlock(0)!).getByRole('button', { name: 'Copy answer' }))
    for (let frame = 0; frame < 2; frame++) await new Promise(resolve => requestAnimationFrame(resolve))
    await expect(runCopy).not.toHaveAttribute('data-copied')
    Object.defineProperty(clipboard, 'writeText', { configurable: true, value: async (text: string) => { copied = text } })
    await userEvent.click(runCopy)
    await waitFor(() => expect(copied).toBe(run.map(item => item.text).join('\n\n')))
    await waitFor(() => expect(runCopy).toHaveAttribute('data-copied', 'true'))
  } finally {
    window.removeEventListener('unhandledrejection', record)
    if (original === undefined) delete (clipboard as unknown as Record<string, unknown>)['writeText']; else Object.defineProperty(clipboard, 'writeText', original)
  }

  // Prose run: one sender line with the time, then the following blocks without a header.
  const runBlocks = run.map((_, index) => runBlock(index)!)
  await expect(within(runBlocks[0]!).getByTestId('message-sender')).toHaveTextContent(caption)
  await expect(within(runBlocks[0]!).getByTestId('message-header')).toContainElement(runBlocks[0]!.querySelector('time'))
  for (const block of runBlocks.slice(1)) {
    await expect(block).toHaveAttribute('data-run-continuation', 'true')
    await expect(block.querySelector('[data-testid="message-header"]')).toBeNull()
  }
  await expect(canvasElement.querySelectorAll('[data-item-id="hierarchy/run-turn"] [data-testid="message-sender"]')).toHaveLength(1)
} }
export const Light1440: Story = { ...Dark1440, args: { scheme: 'light' } }
export const Dark440: Story = { ...Dark1440, args: { narrow: true } }
export const Light440: Story = { ...Dark1440, args: { narrow: true, scheme: 'light' } }

/** C8/E14: compare headerless seat prose without changing the current default before the T pick. */
export const HeaderlessDark1440: Story = { args: { showSeatHeader: false }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findAllByTestId('agent-message')
  await userEvent.click(canvas.getByRole('button', { name: 'Apply selected regression' }))
  await waitFor(() => expect(canvasElement.querySelector('main')).toHaveAttribute('data-revision', '1'))
  for (const item of [rich, ...run]) {
    const answer = canvasElement.querySelector<HTMLElement>(`[data-item-id="${item.id}"]`)!
    await expect(within(answer).getByTestId('markdown')).toBeVisible()
    await expect(answer.querySelector('[data-testid="message-header"]')).toBeNull()
    await expect(within(answer).queryByRole('button', { name: 'Copy answer' })).toBeNull()
    await expect(answer.querySelector('time')).toBeNull()
  }
  for (const item of run.slice(1)) await expect(canvasElement.querySelector(`[data-item-id="${item.id}"]`)).toHaveAttribute('data-run-continuation', 'true')
} }
export const HeaderlessLight1440: Story = { ...HeaderlessDark1440, args: { showSeatHeader: false, scheme: 'light' } }
export const HeaderlessDark440: Story = { ...HeaderlessDark1440, args: { showSeatHeader: false, narrow: true } }
export const HeaderlessLight440: Story = { ...HeaderlessDark1440, args: { showSeatHeader: false, narrow: true, scheme: 'light' } }

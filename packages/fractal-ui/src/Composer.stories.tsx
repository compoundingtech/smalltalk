/** Decided composer: C2 slab · R3 ask while running · M2 agent/thread/file chips plus / commands · K1 recipient readout. */
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fireEvent, userEvent, waitFor, within } from 'storybook/test'
import { Button, TokenFieldValue } from 'react-aria-components'
import { ComposerSession, type ComposerLayout, type ComposerSessionProps, type MentionMode, type RunningBehavior, type TargetMode } from './assistant-ui/composer-session/ComposerSession'
import { createComposerFixture, composerStates, fixtureEffortControls, type ComposerState, type ComposerTarget } from './assistant-ui/composer-session/composer-fixtures'
import { EmbraceComposer, EmbraceComposerToolbar } from './assistant-ui/EmbraceComposer'
import { Composer } from './assistant-ui/composition/Composer'
import { EmbraceRuntimeProvider, type ConversationRuntimeOptions } from './assistant-ui/EmbraceRuntime'
import { serializeDraft, type Draft, type DraftToken, type MentionToken, type SerializedDraft } from './assistant-ui/embrace-composer/draft'
import { developmentMeasurements } from './assistant-ui/perf/measurement'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

type EffortSupport = keyof typeof fixtureEffortControls
interface ComposerArgs {
  scheme: Scheme
  state: ComposerState
  layout: ComposerLayout
  running: RunningBehavior
  mentions: MentionMode
  target: TargetMode
  effort: EffortSupport
}
interface ComposerPolicy { readonly layout: ComposerLayout; readonly running: RunningBehavior; readonly mentions: MentionMode; readonly target: TargetMode }

const recipients: readonly [ComposerTarget, ...ComposerTarget[]] = [
  { ref: 'agent/review', label: 'Review agent for the composer migration', model: 'claude-opus-4' },
  { ref: 'agent/build', label: 'Build agent', model: 'gpt-5' },
  { ref: 'agent/docs', label: 'Docs agent', model: 'claude-sonnet-4' },
]
const models = [...new Set(recipients.flatMap(recipient => recipient.model === undefined ? [] : [recipient.model]))]
const agentNames = [['review', 'Review agent', 'devbox-1'], ['build', 'Build agent', 'devbox-2'], ['docs', 'Docs agent', 'laptop']] as const
const filePaths = ['src/composer/draft.ts', 'src/runtime/draft.ts', 'src/composer/EmbraceComposer.tsx', 'src/transcript/Transcript.tsx', 'docs/composer.md']
const limitedMentions: readonly MentionToken[] = [
  ...agentNames.map(([id, label]) => ({ _tag: 'Mention' as const, ref: `agent/${id}`, family: 'agent', label })),
  ...agentNames.map(([id, label]) => ({ _tag: 'Mention' as const, ref: `agent:${id}`, family: 'thread', label: `${label} conversation` })),
  ...filePaths.map(path => ({ _tag: 'Mention' as const, ref: `file/${path}`, family: 'file', label: path.slice(path.lastIndexOf('/') + 1) })),
]
const allMentions: readonly MentionToken[] = [
  ...limitedMentions,
  ...agentNames.map(([id, label]) => ({ _tag: 'Mention' as const, ref: `terminal/${id}`, family: 'terminal', label: `${label} terminal` })),
  { _tag: 'Mention', ref: 'resource/ci-run-418', family: 'resource', label: 'CI run 418' },
  { _tag: 'Mention', ref: 'mission/composer-cutover', family: 'mission', label: 'Composer cutover' },
]
/** One source-owned distinguishing hint per subject; file hints derive from their directory. */
const mentionHints = new Map<string, string>([
  ...agentNames.map(([id, , host]) => [`agent/${id}`, host] as const),
  ...agentNames.map(([id]) => [`agent:${id}`, `@${id}`] as const),
  ...agentNames.map(([id]) => [`terminal/${id}`, `@${id}`] as const),
  ['resource/ci-run-418', '@build'],
])

function typingDraft(mode: MentionMode): { readonly text: string; readonly draft?: Draft } {
  const subjects = ['agent', 'thread', 'file'].flatMap(family => limitedMentions.filter(token => token.family === family).slice(0, 1))
  const terminal = allMentions.find(token => token.family === 'terminal')
  if (terminal !== undefined) subjects.push(terminal)
  const segments: Draft['segments'][number][] = [{ type: 'token', text: '/title', value: { _tag: 'Command', command: 'title' } }, { type: 'text', text: ' Review composer changes\n' }]
  subjects.forEach((token, index) => {
    segments.push(mode === 'M1' || (mode === 'M2' && token.family === 'terminal')
      ? { type: 'text', text: `@${token.label}` }
      : { type: 'token', text: `@${token.ref}`, value: token })
    segments.push({ type: 'text', text: index === 1 ? '\n' : ' ' })
  })
  segments.push({ type: 'text', text: '\nCheck focus and keep the explanation concise.' })
  if (mode === 'M1') return { text: segments.map(segment => segment.text).join('') }
  const draft = new TokenFieldValue<DraftToken>(segments)
  return { text: serializeDraft(draft).content, draft }
}

const decided: ComposerPolicy = { layout: 'C2', running: 'R3', mentions: 'M2', target: 'K1' }
const policyId = ({ layout, running, mentions, target }: ComposerPolicy) => `${layout} · ${running} · ${mentions} · ${target}`

function ComposerFrame({ policy, state, effort, frameId, composerWidth }: { readonly policy: ComposerPolicy; readonly state: ComposerState; readonly effort: EffortSupport; readonly frameId: string; readonly composerWidth?: number }) {
  const [source] = React.useState(() => createComposerFixture(state, fixtureEffortControls[effort]))
  const snapshot = React.useSyncExternalStore(source.subscribe, source.getSnapshot, source.getSnapshot)
  const seeded = React.useMemo(() => state === 'typing multi-line' ? typingDraft(policy.mentions) : undefined, [state, policy.mentions])
  const session = React.useMemo<ComposerSessionProps>(() => ({
    options: snapshot.options, layout: policy.layout, runningBehavior: policy.running, mentions: policy.mentions, targeting: policy.target,
    initialDraft: seeded?.text ?? source.initialDraft, initialTokenDraft: seeded?.draft, initialImage: source.imageFile,
    recipients, models, limitedMentions, allMentions, mentionHints, failure: snapshot.failure, offline: snapshot.offline,
    effortControl: snapshot.effortControl, queuedEffort: snapshot.queuedEffort,
  }), [snapshot, policy, source, seeded])
  return <section aria-label={`${policyId(policy)} composer`} data-composer-frame={frameId} data-footer-budget-width={composerWidth} data-footer-budget-layout={composerWidth === undefined ? undefined : policy.layout} data-footer-budget-target={composerWidth === undefined ? undefined : policy.target} data-footer-budget-running={composerWidth === undefined ? undefined : policy.running} {...stylex.props(styles.frame, composerWidth !== undefined && styles.budgetFrame(composerWidth))}>
    <header {...stylex.props(styles.frameHeader)}>
      <h3 {...stylex.props(styles.frameTitle)}>{policyId(policy)}</h3>
      <span {...stylex.props(styles.caption)}>{state}</span>
      {snapshot.options.isRunning && <Button onPress={source.finish} {...stylex.props(styles.button)}>Finish run</Button>}
      {snapshot.offline && <Button onPress={source.reconnect} {...stylex.props(styles.button)}>Reconnect</Button>}
    </header>
    <div {...stylex.props(styles.frameBody, composerWidth !== undefined && styles.budgetFrameBody)}>
      <ComposerSession draftKey={`kit-composer.${frameId}.${state}.${effort}`} {...session} />
      <div aria-label="Accepted fixture messages" role="log" {...stylex.props(styles.receipts)}>{snapshot.receipts.map((receipt, index) => {
        const serialized = receipt.message.runConfig?.custom?.embraceDraft as SerializedDraft | undefined
        return <article key={index} {...stylex.props(styles.receipt)}>
          <strong>{receipt.mode === 'steer' ? 'Steering' : 'Accepted'} · {receipt.target?.label ?? 'Recipient unavailable'} · {receipt.target?.model ?? 'Model unavailable'}{receipt.effort !== undefined ? ` · Effort: ${receipt.effort}` : ''}</strong>
          <p {...stylex.props(styles.text)}>{receipt.message.content.filter(part => part.type === 'text').map(part => part.text).join('\n')}</p>
          {serialized?.title !== undefined && <div>Title: {serialized.title}</div>}
          {serialized !== undefined && serialized.mentions.length > 0 && <div>Mentions: {serialized.mentions.join(', ')}</div>}
          {serialized !== undefined && serialized.commands.length > 0 && <div>Commands: {serialized.commands.join(', ')}</div>}
          {receipt.message.attachments !== undefined && receipt.message.attachments.length > 0 && <div>Attachments: {receipt.message.attachments.map(attachment => attachment.name).join(', ')}</div>}
        </article>
      })}</div>
    </div>
  </section>
}

function Surface({ scheme, children, testId }: { readonly scheme: Scheme; readonly children: React.ReactNode; readonly testId?: string }) {
  return <main data-testid={testId} data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>{children}</ThemePortal></main>
}
function SingleComposer(args: ComposerArgs) {
  const policy = { layout: args.layout, running: args.running, mentions: args.mentions, target: args.target }
  return <Surface scheme={args.scheme}><div {...stylex.props(styles.lane)}><ComposerFrame key={`${policyId(policy)}.${args.state}.${args.effort}`} policy={policy} state={args.state} effort={args.effort} frameId="single" /></div></Surface>
}

const meta = {
  title: 'Fractal UI/Composer',
  component: SingleComposer,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Decided composer C2 · R3 · M2 · K1 over assistant-ui native composer state and React Aria TokenField. Native queue, structured mention/command serialization, image attachments, durable local drafts and explicit local fixture send outcomes; no network transport is claimed.' } } },
  args: { scheme: 'dark', state: 'idle', ...decided, effort: 'supported' },
  argTypes: {
    scheme: { options: ['dark', 'light'], control: 'radio' },
    state: { options: composerStates, control: 'select' },
    layout: { options: ['C1', 'C2', 'C3'], control: 'radio' },
    running: { options: ['R1', 'R2', 'R3'], control: 'radio' },
    mentions: { options: ['M1', 'M2', 'M3'], control: 'radio' },
    target: { options: ['K1', 'K2', 'K3'], control: 'radio' },
    effort: { options: Object.keys(fixtureEffortControls), control: 'radio' },
  },
} satisfies Meta<typeof SingleComposer>
export default meta
type Story = StoryObj<typeof meta>

/** Non-interacting fixtures let the source geometry gate measure real composer state, before any play mutates it. */
export const Geometry: StoryObj<ComposerArgs & { compareLayout?: boolean; compareRunning?: boolean; compareTarget?: boolean; compareMentions?: boolean }> = {
  beforeEach: clearDrafts,
  argTypes: { compareLayout: { control: 'boolean' }, compareRunning: { control: 'boolean' }, compareTarget: { control: 'boolean' }, compareMentions: { control: 'boolean' } },
  render: (args: ComposerArgs & { compareLayout?: boolean; compareRunning?: boolean; compareTarget?: boolean; compareMentions?: boolean }) => {
    const axes = [
      { flag: 'compareLayout', field: 'layout', choices: ['C1', 'C2', 'C3'], label: 'Layout comparison' },
      { flag: 'compareRunning', field: 'running', choices: ['R1', 'R2', 'R3'], label: 'While running comparison' },
      { flag: 'compareTarget', field: 'target', choices: ['K1', 'K2', 'K3'], label: 'Target/model comparison' },
      { flag: 'compareMentions', field: 'mentions', choices: ['M1', 'M2', 'M3'], label: 'Mentions comparison' },
    ] as const
    const axis = axes.find(candidate => args[candidate.flag])
    const policy = { layout: args.layout, running: args.running, mentions: args.mentions, target: args.target }
    return <Surface scheme={args.scheme}>{axis === undefined ? <div data-testid="composer-geometry-lane" {...stylex.props(styles.geometryLane(768))}><ComposerFrame policy={policy} state={args.state} effort={args.effort} frameId="single" /></div> : <section role="region" aria-label={axis.label}><div {...stylex.props(styles.gallery)}>{axis.choices.map(choice => <div key={choice} data-testid="composer-geometry-lane" {...stylex.props(styles.geometryLane(768))}><ComposerFrame policy={{ ...policy, [axis.field]: choice }} state={args.state} effort={args.effort} frameId={choice} /></div>)}</div></section>}</Surface>
  },
}

const settle = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const textbox = (canvasElement: HTMLElement) => within(canvasElement).getByRole('textbox')
const receiptLog = (canvasElement: HTMLElement) => within(canvasElement).getByRole('log', { name: 'Accepted fixture messages' })
const clearDrafts = () => { for (const key of Object.keys(localStorage)) if (key.startsWith('kit-composer.')) localStorage.removeItem(key) }
/** RAC shows focus tooltips only in keyboard modality, so reach the control with real Tab presses. */
const tabTo = async (element: HTMLElement) => {
  for (let step = 0; step < 12 && element.ownerDocument.activeElement !== element; step++) await userEvent.tab()
  await expect(element).toHaveFocus()
}

export const Idle: Story = {
  beforeEach: clearDrafts,
  play: async ({ canvasElement }) => {
    const field = textbox(canvasElement)
    // The visible guidance is the textbox description while the draft is empty, then clears.
    const guidance = canvasElement.querySelector<HTMLElement>('[data-testid="composer-placeholder"]')
    await expect(guidance?.textContent).toMatch(/@.*\//)
    await expect(field.getAttribute('aria-describedby')?.split(' ')).toContain(guidance?.id)
    const form = field.closest('form')!
    // C2 stays a column slab even for one line; the container, not the textbox, owns the focus ring.
    await expect(getComputedStyle(form).flexDirection).toBe('column')
    await userEvent.click(field)
    await expect(getComputedStyle(field).outlineStyle).toBe('none')
    await expect(getComputedStyle(form).outlineStyle).toBe('solid')
    await userEvent.keyboard('Ship the composer')
    await waitFor(() => expect(canvasElement.querySelector('[data-testid="composer-placeholder"]')).toBeNull())
    await expect(field.getAttribute('aria-describedby')?.split(' ')).not.toContain(guidance?.id)
    await userEvent.keyboard('{Enter}')
    await waitFor(() => expect(within(receiptLog(canvasElement)).getByText('Ship the composer')).toBeVisible())
    // K1: the receipt carries the readout recipient and model from the run config.
    await expect(within(receiptLog(canvasElement)).getByText(/Accepted · Review agent for the composer migration · claude-opus-4/)).toBeVisible()
  },
}
export const IdleLight: Story = { ...Idle, args: { scheme: 'light' } }
export const TypingMultiLine: Story = {
  beforeEach: clearDrafts,
  args: { state: 'typing multi-line' },
  play: async ({ canvasElement }) => {
    const field = textbox(canvasElement)
    // M2 seeds agent, thread, file and command chips; terminals stay plain text.
    await waitFor(() => expect(field.querySelectorAll('[data-react-aria-token]').length).toBeGreaterThanOrEqual(4))
    await expect(field.textContent).toContain('@Review agent terminal')
    await userEvent.click(within(canvasElement).getByRole('button', { name: 'Send' }))
    const log = within(receiptLog(canvasElement))
    await waitFor(() => expect(log.getByText('Title: Review composer changes')).toBeVisible())
    await expect(log.getByText(/Mentions: agent\/review, agent:review, file\/src\/composer\/draft\.ts/)).toBeVisible()
  },
}
export const Running: Story = {
  beforeEach: clearDrafts,
  args: { state: 'running' },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const page = within(canvasElement.ownerDocument.body)
    // R3: Enter while running asks per message instead of guessing queue versus steer.
    await userEvent.click(textbox(canvasElement))
    await userEvent.keyboard('{End}{Enter}')
    const dialog = await page.findByRole('dialog', { name: 'Choose running message action' })
    await userEvent.click(within(dialog).getByRole('button', { name: 'Queue' }))
    await waitFor(() => expect(canvas.getByText('1 queued')).toBeVisible())
    await userEvent.click(canvas.getByRole('button', { name: 'Finish run' }))
    await waitFor(() => expect(within(receiptLog(canvasElement)).getByText('Include the unread counter in the review.')).toBeVisible())
  },
}
export const QueuedTwice: Story = {
  beforeEach: clearDrafts,
  args: { state: 'queued x2' },
  play: async ({ canvasElement }) => {
    const queue = within(within(canvasElement).getByLabelText('Queued messages'))
    await expect(queue.getByText('2 queued')).toBeVisible()
    await expect(queue.getAllByLabelText('Queued message effort').map(node => node.textContent)).toEqual(['Effort: high', 'Effort: medium'])
    await userEvent.click(queue.getAllByRole('button', { name: /^Remove queued message/ })[0]!)
    await waitFor(() => expect(queue.getByText('1 queued')).toBeVisible())
    await expect(queue.getAllByLabelText('Queued message effort').map(node => node.textContent)).toEqual(['Effort: medium'])
  },
}
export const FailedSendRestored: Story = {
  beforeEach: clearDrafts,
  args: { state: 'failed send (restored)' },
  play: async ({ canvasElement }) => {
    await expect(within(canvasElement).getByRole('alert')).toHaveTextContent('Your draft was restored')
    await waitFor(() => expect(textbox(canvasElement).textContent).toContain('Send the generated review summary.'))
  },
}
export const OfflineDraft: Story = {
  beforeEach: clearDrafts,
  args: { state: 'offline draft' },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await expect(canvas.getByText(/Offline: the draft remains editable and saved locally/)).toBeVisible()
    // Offline is a send capability gate; the draft itself stays editable.
    await expect(textbox(canvasElement)).not.toHaveAttribute('aria-readonly', 'true')
    await expect(canvas.getByRole('button', { name: 'Send' })).toBeDisabled()
    await userEvent.click(canvas.getByRole('button', { name: 'Reconnect' }))
    await waitFor(() => expect(canvas.getByRole('button', { name: 'Send' })).toBeEnabled())
  },
}
export const ImageAttached: Story = {
  beforeEach: clearDrafts,
  args: { state: 'image attached' },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await waitFor(() => expect(canvas.getByRole('img', { name: 'generated-layout.svg' })).toBeVisible())
    await userEvent.click(canvas.getByRole('button', { name: 'Send' }))
    await waitFor(() => expect(within(receiptLog(canvasElement)).getByText('Attachments: generated-layout.svg')).toBeVisible())
  },
}

export const AllStates: Story = {
  beforeEach: clearDrafts,
  render: args => <Surface scheme={args.scheme}><div {...stylex.props(styles.gallery)}>{composerStates.map(state => <ComposerFrame key={state} policy={decided} state={state} effort={args.effort} frameId={`all.${state}`} />)}</div></Surface>,
}
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }

/** Readout and picker tooltips: keyboard and pointer reach the full value once, without native titles. */
export const TargetTooltips: Story = {
  beforeEach: clearDrafts,
  args: { state: 'running' },
  render: args => <Surface scheme={args.scheme}><div {...stylex.props(styles.gallery)}>{(['K1', 'K2', 'K3'] as const).map(target => <div key={target} {...stylex.props(styles.narrow)}><ComposerFrame policy={{ ...decided, target }} state={args.state} effort={args.effort} frameId={`tooltip.${target}`} /></div>)}</div></Surface>,
  play: async ({ canvasElement }) => {
    const page = within(canvasElement.ownerDocument.body)
    const readout = canvasElement.querySelector<HTMLElement>('[data-composer-frame="tooltip.K1"] [data-composer-target-readout]')!
    const expected = readout.dataset.composerTargetReadout!
    await expect(expected).toBe('Review agent for the composer migration · claude-opus-4')
    await userEvent.click(canvasElement.querySelector<HTMLElement>('[data-composer-frame="tooltip.K1"] [role="textbox"]')!)
    await tabTo(readout)
    const tip = await page.findByRole('tooltip')
    await expect(tip).toHaveTextContent(expected)
    const viewport = canvasElement.ownerDocument.documentElement
    const bounds = tip.getBoundingClientRect()
    await expect(bounds.left >= 0 && bounds.top >= 0 && bounds.right <= viewport.clientWidth && bounds.bottom <= viewport.clientHeight).toBe(true)
    // The name stays short; the complete value is announced once, via the tooltip description.
    await expect(readout).toHaveAccessibleName('Recipient and model')
    await expect(readout).toHaveAccessibleDescription(expected)
    await expect(readout.closest('[data-testid="composer-footer"]')!.querySelectorAll('[title]')).toHaveLength(0)
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(page.queryByRole('tooltip')).toBeNull())
    for (const name of ['Select recipient', 'Select model']) {
      const control = within(canvasElement.querySelector<HTMLElement>('[data-composer-frame="tooltip.K3"]')!).getByRole('button', { name })
      const label = control.querySelector('[data-composer-picker-label]')?.textContent
      await userEvent.hover(control)
      await waitFor(() => expect(page.getByRole('tooltip')).toHaveTextContent(label ?? ''))
      await expect(control.querySelectorAll('[title]')).toHaveLength(0)
      await userEvent.unhover(control)
      await waitFor(() => expect(page.queryByRole('tooltip')).toBeNull())
    }
  },
}
export const TargetTooltipsLight: Story = { ...TargetTooltips, args: { scheme: 'light', state: 'running' } }

/** Suggestion popup geometry: 4px above the form, sticky group headers, 28px rows, first result active on open. */
export const MentionPopup: Story = {
  beforeEach: clearDrafts,
  args: { mentions: 'M3' },
  play: async ({ canvasElement }) => {
    const page = within(canvasElement.ownerDocument.body)
    const field = textbox(canvasElement)
    await userEvent.click(field)
    await userEvent.keyboard('@')
    const list = await page.findByRole('listbox', { name: 'Mention a subject' })
    await settle()
    const form = field.closest('form')!.getBoundingClientRect()
    const popover = list.closest('[data-trigger]') ?? list.parentElement!
    const gap = form.top - popover.getBoundingClientRect().bottom
    await expect(gap).toBeGreaterThanOrEqual(3.5)
    await expect(gap).toBeLessThanOrEqual(4.5)
    const groups = [...list.querySelectorAll<HTMLElement>('[role="group"]')]
    await expect(groups.length).toBeGreaterThan(1)
    // Each group previews min(4, floor((308 - 24n) / 28n)) rows so every header and first row fit unscrolled.
    const previewLimit = Math.max(1, Math.min(4, Math.floor((308 - 24 * groups.length) / (28 * groups.length))))
    for (const group of groups) {
      const rows = group.querySelectorAll<HTMLElement>('[role="option"]')
      await expect(rows.length).toBeGreaterThan(0)
      await expect(rows.length).toBeLessThanOrEqual(previewLimit)
      for (const row of rows) {
        const height = row.getBoundingClientRect().height
        await expect(height >= 28 && height <= 32).toBe(true)
      }
      const header = canvasElement.ownerDocument.getElementById(group.getAttribute('aria-labelledby') ?? '')
      await expect(header === null ? undefined : getComputedStyle(header).position).toBe('sticky')
    }
    const first = list.querySelector<HTMLElement>('[role="option"]')!
    await expect(first).toHaveAttribute('data-focused', 'true')
    const firstLabel = first.querySelector('[slot="label"]')!.textContent!
    await userEvent.keyboard('src')
    await waitFor(() => expect(list.querySelectorAll('[role="group"]').length).toBeLessThan(groups.length))
    for (const row of list.querySelectorAll('[role="option"]')) await expect(row.textContent).toContain('src')
    await userEvent.keyboard('{Backspace}{Backspace}{Backspace}{Enter}')
    await waitFor(() => expect(field.querySelectorAll('[data-react-aria-token]')).toHaveLength(1))
    await expect(field.textContent).toContain(firstLabel)
  },
}
export const MentionPopupLight: Story = { ...MentionPopup, args: { scheme: 'light', mentions: 'M3' } }
export const SlashCommands: Story = {
  beforeEach: clearDrafts,
  play: async ({ canvasElement }) => {
    const page = within(canvasElement.ownerDocument.body)
    await userEvent.click(textbox(canvasElement))
    await userEvent.keyboard('/')
    const list = await page.findByRole('listbox', { name: 'Commands' })
    const row = within(list).getByRole('option', { name: /\/title/ })
    await expect(row).toHaveTextContent('Use the first line as the message title')
    await userEvent.keyboard('{Enter}')
    await waitFor(() => expect(textbox(canvasElement).querySelectorAll('[data-react-aria-token]')).toHaveLength(1))
  },
}

/** IME composition owns Enter: confirming a candidate never sends; Enter after composition does. */
export const ImeComposition: Story = {
  beforeEach: clearDrafts,
  play: async ({ canvasElement }) => {
    const field = textbox(canvasElement)
    await userEvent.click(field)
    await userEvent.keyboard('nihon')
    fireEvent.compositionStart(field)
    fireEvent.keyDown(field, { key: 'Enter', code: 'Enter', keyCode: 229, isComposing: true })
    fireEvent.compositionEnd(field)
    await settle()
    await expect(within(receiptLog(canvasElement)).queryByText('nihon')).toBeNull()
    await userEvent.keyboard('{Enter}')
    await waitFor(() => expect(within(receiptLog(canvasElement)).getByText('nihon')).toBeVisible())
  },
}
/** Shift+Enter inserts a newline; the composer grows instead of sending. */
export const ShiftEnterNewline: Story = {
  beforeEach: clearDrafts,
  play: async ({ canvasElement }) => {
    const field = textbox(canvasElement)
    await userEvent.click(field)
    await userEvent.keyboard('first line{Shift>}{Enter}{/Shift}second line')
    await expect(field.textContent).toContain('second line')
    await expect(receiptLog(canvasElement).children).toHaveLength(0)
  },
}

function RemountingComposer(args: ComposerArgs) {
  const [mount, setMount] = React.useState(0)
  return <Surface scheme={args.scheme}>
    <div {...stylex.props(styles.lane)}>
      <Button onPress={() => setMount(value => value + 1)} {...stylex.props(styles.button)}>Remount composer</Button>
      <ComposerFrame key={mount} policy={decided} state="idle" effort={args.effort} frameId="durable" />
    </div>
  </Surface>
}
/** Drafts persist per key across remounts, including structured chips. */
export const DurableDraft: Story = {
  beforeEach: clearDrafts,
  render: args => <RemountingComposer {...args} />,
  play: async ({ canvasElement }) => {
    const page = within(canvasElement.ownerDocument.body)
    await userEvent.click(textbox(canvasElement))
    await userEvent.keyboard('Keep this @')
    await page.findByRole('listbox', { name: 'Mention a subject' })
    await userEvent.keyboard('{Enter} across remounts')
    await waitFor(() => expect(Object.keys(localStorage).some(key => key.startsWith('kit-composer.durable') && localStorage.getItem(key)!.includes('across remounts'))).toBe(true), { timeout: 2000 })
    await userEvent.click(within(canvasElement).getByRole('button', { name: 'Remount composer' }))
    await waitFor(() => expect(textbox(canvasElement).textContent).toContain('across remounts'))
    await expect(textbox(canvasElement).querySelectorAll('[data-react-aria-token]')).toHaveLength(1)
  },
}

const effortFrames = (['supported', 'unsupported', 'unknown', 'queued'] as const).map(id => ({
  id, effort: id === 'queued' ? 'supported' as const : id, state: id === 'queued' ? 'queued x2' as const : 'typing multi-line' as const,
}))
/** Effort is conversation-frame metadata: one-shot by default, optionally kept for the next messages. */
export const EffortSupported: Story = {
  beforeEach: clearDrafts,
  args: { state: 'typing multi-line', effort: 'supported' },
  play: async ({ canvasElement }) => {
    const page = within(canvasElement.ownerDocument.body)
    const picker = within(canvasElement).getByRole('button', { name: 'Select effort' })
    await expect(picker).toHaveTextContent('Medium')
    await userEvent.click(picker)
    const high = await page.findByRole('menuitemradio', { name: 'High' })
    await expect(page.getByRole('menuitemradio', { name: 'Medium' })).toHaveAttribute('aria-checked', 'true')
    await expect(page.getByText('Default')).toBeVisible()
    await expect(page.getByRole('menuitemcheckbox', { name: 'Keep for next messages' })).toHaveAttribute('aria-checked', 'false')
    // Frame budget: one choice commits the value subtree exactly once without dropping frames.
    const engine = developmentMeasurements?.createMeasurementEngine()
    const handle = engine?.beginMeasure()
    await userEvent.click(high)
    if (engine !== undefined && handle !== undefined) {
      try {
        const measure = await engine.endMeasure(handle, { settleFrames: 2 })
        await expect(measure.frameDrops).toBe(0)
        await expect(measure.debugDelta['Renders.EffortValue']).toBe(1)
      } finally { engine.dispose() }
    }
    await expect(high).toHaveAttribute('aria-checked', 'true')
    await expect(page.getAllByRole('menuitemradio').filter(item => item.getAttribute('aria-checked') === 'true')).toHaveLength(1)
    await userEvent.keyboard('{Escape}')
    await userEvent.click(within(canvasElement).getByRole('button', { name: 'Send' }))
    await waitFor(() => expect(within(receiptLog(canvasElement)).getByText(/Effort: high/)).toBeVisible())
    await expect(picker).toHaveTextContent('Medium')
  },
}
export const EffortUnsupported: Story = {
  beforeEach: clearDrafts,
  args: { state: 'typing multi-line', effort: 'unsupported' },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await expect(canvas.getByRole('button', { name: 'Effort unsupported' })).toBeDisabled()
    await userEvent.click(textbox(canvasElement))
    await tabTo(canvas.getByRole('group', { name: 'Effort unsupported reason' }))
    await expect(await within(canvasElement.ownerDocument.body).findByRole('tooltip')).toHaveTextContent('This harness does not support per-message effort.')
  },
}
export const EffortUnknown: Story = {
  beforeEach: clearDrafts,
  args: { state: 'typing multi-line', effort: 'unknown' },
  play: async ({ canvasElement }) => {
    await expect(within(canvasElement).queryByRole('button', { name: 'Select effort' })).toBeNull()
    await userEvent.click(within(canvasElement).getByRole('button', { name: 'Send' }))
    await waitFor(() => expect(receiptLog(canvasElement).children).toHaveLength(1))
    await expect(receiptLog(canvasElement).textContent).not.toContain('Effort:')
  },
}
export const EffortAllStates: Story = {
  beforeEach: clearDrafts,
  render: args => <Surface scheme={args.scheme}><div {...stylex.props(styles.gallery)}>{effortFrames.map(frame => <ComposerFrame key={frame.id} policy={decided} state={frame.state} effort={frame.effort} frameId={`effort.${frame.id}`} />)}</div></Surface>,
}

const footerBudgetFrames = ([320, 360, 451] as const).flatMap(composerWidth =>
  (['C1', 'C2', 'C3'] as const).flatMap(layout => (['K1', 'K2', 'K3'] as const).flatMap(target => (['R1', 'R2', 'R3'] as const).map(running => ({
    composerWidth, policy: { layout, target, running, mentions: 'M2' } satisfies ComposerPolicy, frameId: `footer-budget.${composerWidth}.${layout}.${target}.${running}`,
  })))))
/** The declared width is the real form width; every footer control stays inside its running composer. */
export const FooterWidthBudget: Story = {
  beforeEach: clearDrafts,
  args: { state: 'running', effort: 'supported' },
  render: args => <Surface scheme={args.scheme} testId="composer-footer-width-budget"><div aria-label="Footer width budget previews" {...stylex.props(styles.budgetPreviews)}>{footerBudgetFrames.map(frame => <ComposerFrame key={frame.frameId} {...frame} state="running" effort="supported" />)}</div></Surface>,
  play: async ({ canvasElement }) => {
    await canvasElement.ownerDocument.fonts.ready
    let previousGeometry: number[] | undefined
    // Retry through React/ResizeObserver updates, then require two matching measurements.
    await waitFor(() => {
      const cards = [...canvasElement.querySelectorAll<HTMLElement>('[data-footer-budget-width]')]
      expect(cards).toHaveLength(81)
      const keyOf = (card: HTMLElement) => `${card.dataset.footerBudgetWidth}.${card.dataset.footerBudgetLayout}.${card.dataset.footerBudgetTarget}.${card.dataset.footerBudgetRunning}`
      const geometry: number[] = []
      for (const card of cards) {
        expect(card.querySelectorAll('[data-testid="kit-composer"]')).toHaveLength(1)
        const footers = card.querySelectorAll<HTMLElement>('[data-testid="composer-footer"]')
        expect(footers).toHaveLength(1)
        const form = footers[0]!.closest('form')!
        const bounds = form.getBoundingClientRect()
        expect(bounds.width).toBeCloseTo(Number(card.dataset.footerBudgetWidth), 2)
        geometry.push(bounds.x, bounds.y, bounds.width, bounds.height)
        const controls = [...footers[0]!.querySelectorAll<HTMLElement>('button, [role="button"], [data-composer-target-readout]')].filter(control => {
          const rect = control.getBoundingClientRect()
          const css = getComputedStyle(control)
          return rect.width > 0 && rect.height > 0 && css.display !== 'none' && css.visibility === 'visible'
        })
        expect(controls.length).toBeGreaterThan(0)
        for (const control of controls) {
          const rect = control.getBoundingClientRect()
          const name = `${keyOf(card)}: ${control.ariaLabel ?? control.textContent}`
          expect(rect.left, `${name} left edge`).toBeGreaterThanOrEqual(bounds.left - 1)
          expect(rect.right, `${name} right edge`).toBeLessThanOrEqual(bounds.right + 1)
          expect(rect.top, `${name} top edge`).toBeGreaterThanOrEqual(bounds.top - 1)
          expect(rect.bottom, `${name} bottom edge`).toBeLessThanOrEqual(bounds.bottom + 1)
          geometry.push(rect.x, rect.y, rect.width, rect.height)
        }
      }
      const previous = previousGeometry
      previousGeometry = geometry
      expect(geometry).toEqual(previous)
    }, { timeout: 10_000 })
  },
}
export const FooterWidthBudgetLight: Story = { ...FooterWidthBudget, args: { scheme: 'light', state: 'running', effort: 'supported' } }

/** C1 grows from a pill into a slab once the draft wraps or breaks lines; C3 becomes a slab while focused. */
export const LayoutShapes: Story = {
  beforeEach: clearDrafts,
  render: args => <Surface scheme={args.scheme}><div {...stylex.props(styles.gallery)}>{(['C1', 'C2', 'C3'] as const).map(layout => <div key={layout} {...stylex.props(styles.narrow)}><ComposerFrame policy={{ ...decided, layout }} state="idle" effort={args.effort} frameId={`layout.${layout}`} /></div>)}</div></Surface>,
  play: async ({ canvasElement }) => {
    const formOf = (layout: ComposerLayout) => canvasElement.querySelector<HTMLElement>(`[data-composer-frame="layout.${layout}"] form`)!
    await expect(getComputedStyle(formOf('C1')).flexDirection).toBe('row')
    await expect(getComputedStyle(formOf('C2')).flexDirection).toBe('column')
    await expect(getComputedStyle(formOf('C3')).flexDirection).toBe('row')
    await userEvent.click(within(formOf('C3')).getByRole('textbox'))
    await waitFor(() => expect(getComputedStyle(formOf('C3')).flexDirection).toBe('column'))
    await userEvent.click(within(formOf('C1')).getByRole('textbox'))
    await userEvent.keyboard('one{Shift>}{Enter}{/Shift}two')
    await waitFor(() => expect(getComputedStyle(formOf('C1')).flexDirection).toBe('column'))
  },
}

/** Direct kit consumer: changing a label re-measures the footer without changing the outer width. */
function LabelResize({ scheme }: { readonly scheme: Scheme }) {
  const [source] = React.useState(() => createComposerFixture('idle'))
  const [longLabel, setLongLabel] = React.useState(false)
  const target = { ref: 'fixture:recipient', label: longLabel ? 'A considerably longer recipient label' : 'Me' }
  return <Surface scheme={scheme}><div {...stylex.props(styles.narrow)}>
    <Button onPress={() => setLongLabel(value => !value)} {...stylex.props(styles.button)}>Change recipient label</Button>
    <EmbraceRuntimeProvider options={source.getSnapshot().options}>
      <EmbraceComposer variant="C1" plainText toolbar={<EmbraceComposerToolbar target={target} recipients={[target]} models={[]} onTargetChange={() => {}} effort={{ control: fixtureEffortControls.unsupported, value: undefined, pinned: false, onChange: () => {}, onPinnedChange: () => {} }} />} />
    </EmbraceRuntimeProvider>
  </div></Surface>
}
export const KitLabelResize: Story = {
  render: args => <LabelResize scheme={args.scheme} />,
  play: async ({ canvasElement }) => {
    const form = canvasElement.querySelector('form')!
    const width = form.getBoundingClientRect().width
    await userEvent.click(within(canvasElement).getByRole('button', { name: 'Change recipient label' }))
    await waitFor(() => expect(canvasElement.querySelector('[data-composer-target-readout]')).toHaveAttribute('data-composer-target-readout', 'A considerably longer recipient label'))
    await settle()
    await expect(form.getBoundingClientRect().width).toBeCloseTo(width, 1)
    const footer = canvasElement.querySelector<HTMLElement>('[data-testid="composer-footer"]')!.getBoundingClientRect()
    await expect(footer.right).toBeLessThanOrEqual(form.getBoundingClientRect().right + 1)
  },
}

/** Plain text adapter used by workbench thread panes: session-scoped draft, queue while running, explicit stop. */
function PlainAdapter({ scheme }: { readonly scheme: Scheme }) {
  const [sent, setSent] = React.useState<readonly string[]>([])
  const [running, setRunning] = React.useState(false)
  // The host owns run state; the native runtime mirrors it so Stop maps to a real cancel capability.
  const options = React.useMemo<ConversationRuntimeOptions>(() => ({ messages: [], isRunning: running, onNew: async () => {}, onCancel: async () => setRunning(false) }), [running])
  return <Surface scheme={scheme}><div {...stylex.props(styles.lane)}>
    <EmbraceRuntimeProvider options={options}>
      <Composer agent="Review agent" folder="fractal-ui" branch="composer-cutover" draftKey="kit-composer.plain-adapter" running={running}
        onSend={text => { setSent(previous => [...previous, `Sent: ${text}`]); setRunning(true) }}
        onSteer={text => setSent(previous => [...previous, `Steered: ${text}`])}
        onStop={() => setRunning(false)} />
    </EmbraceRuntimeProvider>
    <div aria-label="Adapter messages" role="log" {...stylex.props(styles.receipts)}>{sent.map((line, index) => <p key={index} {...stylex.props(styles.receipt)}>{line}</p>)}</div>
  </div></Surface>
}
export const PlainTextAdapter: Story = {
  beforeEach: () => { try { sessionStorage.removeItem('composition.draft.kit-composer.plain-adapter') } catch { /* Storage may be disabled. */ } },
  render: args => <PlainAdapter scheme={args.scheme} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const field = canvas.getByTestId('composer-input')
    await userEvent.click(field)
    await userEvent.keyboard('First message{Enter}')
    await waitFor(() => expect(canvas.getByText('Sent: First message')).toBeVisible())
    // While running, Enter queues locally; Stop restores queued text into the draft.
    await userEvent.keyboard('Queued follow-up{Enter}')
    await waitFor(() => expect(canvas.getByRole('status')).toHaveTextContent('1 message will send after run'))
    await userEvent.click(canvas.getByRole('button', { name: 'Stop' }))
    await waitFor(() => expect(field).toHaveValue('Queued follow-up'))
    await expect(canvas.getByTestId('composer-context')).toHaveTextContent('fractal-ui·composer-cutover')
  },
}

const styles = stylex.create({
  root: { minHeight: '100vh', width: '100%', boxSizing: 'border-box', padding: s.xl, display: 'flex', flexDirection: 'column', gap: s.xl, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  lane: { width: '100%', maxWidth: g.modalMax, display: 'flex', flexDirection: 'column', gap: s.md },
  geometryLane: (width: number) => ({ width, maxWidth: '100%', minWidth: 0 }),
  narrow: { width: g.tooltipMax, minWidth: 0 },
  gallery: { display: 'flex', flexWrap: 'wrap', alignItems: 'flex-start', gap: s.xl },
  budgetPreviews: { display: 'flex', flexWrap: 'wrap', alignItems: 'flex-start', gap: s.md },
  budgetFrame: (width: number) => ({ width: `calc(${width}px + ${s.md} + ${s.md} + ${g.hairline} + ${g.hairline})`, boxSizing: 'border-box', flexShrink: 0, overflow: 'visible' }),
  budgetFrameBody: { overflow: 'visible' },
  frame: { minWidth: 0, display: 'flex', flexDirection: 'column', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, backgroundColor: surface.sidebar },
  frameHeader: { display: 'flex', alignItems: 'center', gap: s.md, flexWrap: 'wrap', padding: s.md, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  frameTitle: { margin: 0, fontSize: t.uiSize, lineHeight: t.uiLeading },
  caption: { color: ink.fgMuted, flexGrow: 1 },
  frameBody: { minWidth: 0, padding: s.md, display: 'flex', flexDirection: 'column', gap: s.xl },
  button: { display: 'inline-flex', justifyContent: 'center', alignItems: 'center', alignSelf: 'flex-start', gap: s.xs, minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  receipts: { display: 'flex', flexDirection: 'column', gap: s.md },
  receipt: { margin: 0, padding: s.md, backgroundColor: surface.rowActive, borderRadius: r.sm, fontSize: t.metaSize, lineHeight: t.metaLeading, overflowWrap: 'anywhere' },
  text: { margin: 0, marginBlock: s.sm, whiteSpace: 'pre-wrap' },
})

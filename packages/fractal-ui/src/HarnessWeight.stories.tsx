import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within } from 'storybook/test'
import { HarnessNotice } from './assistant-ui/composition/HarnessNotice'
import { UnsupportedRecord } from './assistant-ui/composition/UnsupportedRecord'
import { harnessWeightExamples } from './assistant-ui/composition/HarnessNotice.fixtures'
import type { HarnessWeight } from './assistant-ui/embrace-data/model'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

/** q106 pairing: the production HarnessNotice at both weights over the same fictional events. */
const variants: readonly { readonly weight: HarnessWeight; readonly name: string; readonly note: string }[] = [
  { weight: 'row', name: 'B1 · Quiet rows', note: 'Low interruption; longer payloads need deliberate disclosure.' },
  { weight: 'card', name: 'B2 · Compact cards', note: 'Clearest event boundaries; costs more vertical space.' },
]

export function HarnessWeightComparison({ scheme = 'dark' }: { readonly scheme?: Scheme }) {
  return <section aria-label="Harness event weight comparison" data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    {variants.map(variant => <section key={variant.weight} aria-label={variant.name} data-testid={`harness-weight-${variant.weight}`} {...stylex.props(styles.column)}>
      <header {...stylex.props(styles.header)}><h2 {...stylex.props(styles.title)}>{variant.name}</h2><p {...stylex.props(styles.note)}>{variant.note}</p></header>
      <p {...stylex.props(styles.prose)}>The retry change is ready for review. I’m collecting the review and background check results.</p>
      <div {...stylex.props(styles.list)}>{harnessWeightExamples.map(item => <HarnessNotice key={item.id} item={item} weight={variant.weight} />)}</div>
    </section>)}
  </section>
}

const meta = { title: 'Fractal/Explore/Harness event weight', component: HarnessWeightComparison, excludeStories: ['HarnessWeightComparison'], parameters: { layout: 'fullscreen' }, args: { scheme: 'dark' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof HarnessWeightComparison>
export default meta
type Story = StoryObj<typeof meta>

export const Dark: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    for (const weight of ['row', 'card']) {
      const column = within(canvas.getByTestId(`harness-weight-${weight}`))
      await expect(column.getAllByTestId('harness-notice')).toHaveLength(harnessWeightExamples.length)
      const failedJob = column.getAllByTestId('harness-notice').find(node => node.dataset.itemId === 'weight/failed-job')!
      await expect(within(failedJob).getByTestId('harness-kind')).toHaveTextContent('failed')
      const completedJob = column.getAllByTestId('harness-notice').find(node => node.dataset.itemId === 'weight/job')!
      await expect(getComputedStyle(within(failedJob).getByTestId('harness-kind')).color).not.toBe(getComputedStyle(within(completedJob).getByTestId('harness-kind')).color)
      await expect(column.queryByText(/<system-notice>|<irc|<task-result|## Goal/)).toBeNull()
    }
    const read = within(canvas.getByTestId('harness-weight-row')).getByRole('button', { name: 'Read summary' })
    await userEvent.click(read)
    await expect(read).toHaveAttribute('aria-expanded', 'true')
  },
}
export const Light: Story = { ...Dark, args: { scheme: 'light' } }

/** F2 marker stays a single quiet row; only Inspect reveals the capped diagnostic preview. */
export const UnsupportedDark: Story = {
  render: ({ scheme }) => <section data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><div {...stylex.props(styles.column)}>
    <p {...stylex.props(styles.prose)}>The other transcript entries remain readable.</p>
    <UnsupportedRecord item={{ _tag: 'Unsupported', id: 'unsupported/story', harness: 'omp', recordKind: 'model_usage', reason: 'unrecognized-record', at: '2026-01-15T12:30:00Z', inspect: { text: '{\n  \"token\": \"[redacted]\",\n  \"message\": \"capped preview…\"\n}', bytes: 8192, truncated: true } }} />
  </div></section>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const marker = within(canvas.getByTestId('unsupported-record'))
    const trigger = marker.getByRole('button', { name: 'Inspect' })
    await expect(trigger).toHaveAttribute('aria-expanded', 'false')
    await expect(marker.getByText(/capped preview/)).not.toBeVisible()
    await expect(marker.getByText('8,192 bytes · truncated')).not.toBeVisible()
    await userEvent.click(trigger)
    await expect(trigger).toHaveAttribute('aria-expanded', 'true')
    await expect(marker.getByText('8,192 bytes · truncated')).toBeVisible()
  },
}
export const UnsupportedLight: Story = { ...UnsupportedDark, args: { scheme: 'light' } }

/** Native jobs keep unknown and cancellation distinct from success at both weights. */
export const JobOutcomesDark: Story = {
  render: ({ scheme }) => <section data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    {variants.map(({ weight, name }) => <section key={weight} aria-label={name} {...stylex.props(styles.column)}>
      <h2 {...stylex.props(styles.title)}>{name}</h2>
      {(['running', 'completed', 'failed', 'cancelled', 'unknown'] as const).map(state => <HarnessNotice key={state} weight={weight} item={{
        _tag: 'Event', id: `job-outcome/${weight}/${state}`, kind: 'background-job', title: `Background job ${state}`, text: 'Native job', sender: { kind: 'harness', label: 'OMP' }, data: {},
        notice: { _tag: 'BackgroundJob', jobId: state, state, payload: '', raw: '' },
      }} />)}
    </section>)}
  </section>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    for (const state of ['running', 'completed', 'failed', 'cancelled', 'unknown']) {
      await expect(canvas.getAllByTestId('harness-kind').filter(node => node.textContent === state)).toHaveLength(2)
    }
  },
}
export const JobOutcomesLight: Story = { ...JobOutcomesDark, args: { scheme: 'light' } }

const styles = stylex.create({
  root: { display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(22rem, 1fr))', gap: s.section, minHeight: '100vh', padding: s.section, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  column: { display: 'flex', flexDirection: 'column', gap: s.lg, minWidth: 0 },
  header: { paddingBottom: s.md, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { margin: 0, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: 600 },
  note: { margin: 0, color: ink.fgMuted, fontSize: t.metaSize },
  prose: { margin: 0, fontSize: t.bodySize, lineHeight: t.bodyLeading, color: ink.fgSoft },
  list: { display: 'flex', flexDirection: 'column', gap: s.md },
})

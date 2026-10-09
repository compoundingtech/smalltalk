import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { Transcript } from './assistant-ui/composition/Transcript'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

function WorkFocus({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <section data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <Button>Before work log</Button>
    <WorkLogV1 turn={{ calls: [], durationMs: undefined, running: true, failed: false, interrupted: false }} hideLiveRow expandedBody={Array.from({ length: 10 }, (_, index) => <ThinkingEntry key={index} text={`Check synthetic step ${index + 1} before proceeding.`} />)} />
    <Button>After work log</Button>
  </section>
}
const styles = stylex.create({ root: { padding: s.xl, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, minHeight: '100vh' } })
const meta = { title: 'Fractal UI/Small Fixes', component: WorkFocus, args: { scheme: 'dark' }, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof WorkFocus>
export default meta
type Story = StoryObj<typeof meta>
export const WorkLogRovingFocus: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const before = canvas.getByRole('button', { name: 'Before work log' })
  const after = canvas.getByRole('button', { name: 'After work log' })
  const rows = canvas.getAllByRole('button', { name: 'Thinking' })
  before.focus()
  await userEvent.tab()
  await expect(rows[0]).toHaveFocus()
  await userEvent.keyboard('{ArrowDown}')
  await expect(rows[1]).toHaveFocus()
  await userEvent.keyboard('{Enter}')
  await expect(rows[1]).toHaveAttribute('aria-expanded', 'true')
  await userEvent.keyboard('{ArrowUp}')
  await expect(rows[0]).toHaveFocus()
  await userEvent.tab()
  await expect(after).toHaveFocus()
  await userEvent.tab({ shift: true })
  await expect(rows[0]).toHaveFocus()
  await userEvent.tab({ shift: true })
  await expect(before).toHaveFocus()
} }
export const WorkLogRovingFocusLight: Story = { ...WorkLogRovingFocus, args: { scheme: 'light' } }

const loadingNow = Date.parse('2026-01-15T12:00:08Z')
function LoadingConversation({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: async () => {} }}><Transcript title="Synthetic conversation" turns={[]} sync={{ _tag: 'Connecting', attempt: 1, since: loadingNow - 8000 }} now={loadingNow} observedAt={loadingNow - 8000} /></EmbraceRuntimeProvider></section>
}
export const SingleLoadingLabel: Story = { render: args => <LoadingConversation {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const placeholder = await canvas.findByTestId('transcript-placeholder')
  await expect(within(placeholder).getAllByText(/Loading conversation/)).toHaveLength(1)
  await expect(within(placeholder).getAllByRole('status')).toHaveLength(1)
  await expect(placeholder).not.toHaveAttribute('aria-label', 'Loading conversation')
  await expect(within(placeholder).getByTestId('sync-line')).toHaveAttribute('data-sync-visible', 'true')
} }
export const SingleLoadingLabelLight: Story = { ...SingleLoadingLabel, args: { scheme: 'light' } }

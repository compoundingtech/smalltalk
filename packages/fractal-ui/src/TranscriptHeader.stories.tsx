import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { geometryVars as g, surfaceVars as surface, spaceVars as s } from './assistant-ui/composition-tokens.stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, within } from 'storybook/test'
import { Transcript } from './assistant-ui/composition/Transcript.tsx'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime.tsx'

const now = Date.parse('2026-01-15T12:00:00Z')
const styles = stylex.create({
  root: { width: g.polishWide, maxWidth: '100%', backgroundColor: surface.canvas, padding: s.md },
  narrow: { width: g.polishNarrow },
})
const HeaderSurface = ({ scheme = 'dark', narrow = false }: { scheme?: Scheme; narrow?: boolean }) => <main {...stylex.props(styles.root, narrow && styles.narrow, ...baselineTheme, scheme === 'light' && lightTheme)}>{[false, true].map(showHeader => <section key={String(showHeader)} aria-label={showHeader ? 'Standalone control' : 'Host-owned header'} style={{ height: 300 }}>
  {!showHeader && <header role="group" aria-label="Workspace header">Row projection</header>}
  <EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: async () => {} }}>
    <Transcript title="Row projection" turns={[]} showHeader={showHeader} sync={{ _tag: 'Live', since: now }} now={now} observedAt={now} />
  </EmbraceRuntimeProvider>
</section>)}</main>
const meta = { title: 'Fractal UI/Transcript header ownership', component: HeaderSurface, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', narrow: false } } satisfies Meta<typeof HeaderSurface>
export default meta
type Story = StoryObj<typeof meta>
export const Dark1440: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const hosted = within(canvas.getByRole('region', { name: 'Host-owned header' }))
  await expect(hosted.queryByTestId('transcript-header')).toBeNull()
  await expect(hosted.getByRole('group', { name: 'Workspace header' })).toBeVisible()
  await expect(hosted.getByTestId('transcript-scroll')).toBeVisible()
  // Failing control: standalone presentation retains the band by default.
  await expect(within(canvas.getByRole('region', { name: 'Standalone control' })).getByTestId('transcript-header')).toBeVisible()
  canvasElement.dataset.proof = 'passed'
} }
export const Light1440: Story = { ...Dark1440, args: { scheme: 'light' } }
export const Dark440: Story = { ...Dark1440, args: { narrow: true } }
export const Light440: Story = { ...Dark1440, args: { narrow: true, scheme: 'light' } }
export const AllStates: Story = { ...Dark1440 }

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fireEvent, userEvent, waitFor, within } from 'storybook/test'
import { HumanTime, HumanClockProvider, fixedClock, systemClock, type HumanClock } from './HumanTime'
import { civilDate } from './human-time'
import { baselineTheme } from '../assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from '../assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as text, spaceVars as s, typeVars as t } from '../assistant-ui/composition-tokens.stylex'
import { tooltipFor } from '../story-play'

const now = Date.parse('2026-10-12T16:00:00Z')
const zone = { locale: 'en-GB', timeZone: 'America/New_York' }
const at = now - 180_000
const exact = 'Mon, 12 Oct 2026, 11:57:00 GMT-4'
const day = civilDate('2026-10-12', zone)!
const hostText = stylex.createTheme(text, { fg: '#123456' })
const hostSurface = stylex.createTheme(surface, { canvas: '#f1eadb', raised: '#f1eadb' })

function HumanTimeStory({ scheme = 'dark', width = 440, host = false, live = false, browserClock = false, instantTooltips = true }: { scheme?: Scheme; width?: number; host?: boolean; live?: boolean; browserClock?: boolean; instantTooltips?: boolean }) {
  // A fake one-second timer clock keeps the live story deterministic without changing global timers.
  const [timer] = React.useState(() => {
    let value = now
    const listeners = new Set<() => void>()
    const clock: HumanClock = { now: () => value, subscribe: listener => { listeners.add(listener); return () => { listeners.delete(listener) } } }
    return { clock, advance: () => { value += 1_000; for (const listener of listeners) listener() } }
  })
  // Strict plays hover without the delay; DefaultHoverDelay keeps the component's default.
  const hoverDelay = instantTooltips ? 0 : undefined
  return <main data-testid="human-time-story" data-scheme={scheme} {...stylex.props(styles.root, styles.width(width), ...baselineTheme, scheme === 'light' && lightTheme, host && hostText, host && hostSurface)}>
    <HumanClockProvider clock={browserClock ? systemClock : live ? timer.clock : fixedClock(now)} zone={zone}>
      <h1>Human time</h1>
      <dl {...stylex.props(styles.list)}>
        <dt>Relative</dt><dd><HumanTime at={at} data-testid="relative" aria-label="Updated" hoverDelay={hoverDelay} /></dd>
        <dt>Clock</dt><dd><HumanTime at={at} kind="clock" data-testid="clock" hoverDelay={hoverDelay} /></dd>
        <dt>Elapsed</dt><dd><HumanTime at={at} kind="elapsed" data-testid="elapsed" hoverDelay={hoverDelay} /></dd>
        <dt>Date only</dt><dd><HumanTime at={day} kind="day" data-testid="day" hoverDelay={hoverDelay} /></dd>
        <dt>Unreported</dt><dd><HumanTime at={NaN} data-testid="unreported" hoverDelay={hoverDelay} /></dd>
      </dl>
      {live && <button onClick={timer.advance}>Advance fake timer by 1 second</button>}
      <span data-testid="tooltip-colors" {...stylex.props(styles.colors)} aria-hidden="true" />
    </HumanClockProvider>
  </main>
}
const meta = {
  title: 'Fractal UI/Human time', component: HumanTimeStory, parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', width: 440, instantTooltips: true },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, width: { options: [440, 1440], control: 'select' }, instantTooltips: { control: 'boolean' } },
} satisfies Meta<typeof HumanTimeStory>
export default meta
type Story = StoryObj<typeof meta>

// The time is deliberately not focusable (its text is the aria-description), so this is the
// kit's one hover case; it runs with the hover delay at 0 to stay deterministic under load.
const assertStates: NonNullable<Story['play']> = async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  for (const [id, label, dateTime, description] of [
    ['relative', '3m ago', '2026-10-12T15:57:00.000Z', exact],
    ['clock', '11:57', '2026-10-12T15:57:00.000Z', exact],
    ['elapsed', '3m', '2026-10-12T15:57:00.000Z', `Since ${exact}`],
    ['day', 'today', '2026-10-12', 'Mon, 12 Oct 2026'],
  ]) {
    const time = canvas.getByTestId(id!)
    await expect(time).toHaveTextContent(label!)
    await expect(time).toHaveAttribute('datetime', dateTime!)
    await expect(time).toHaveAttribute('aria-description', description!)
    // The tooltip becomes the description only while mounted; either way it is announced once.
    await expect(time).not.toHaveAttribute('aria-describedby')
    await expect(time).toHaveAccessibleDescription(description!)
    await userEvent.hover(time)
    const tooltip = await tooltipFor(time, description!)
    await expect(time).toHaveAccessibleDescription(description!)
    const colors = getComputedStyle(canvas.getByTestId('tooltip-colors'))
    await expect(getComputedStyle(tooltip).backgroundColor).toBe(colors.backgroundColor)
    await expect(getComputedStyle(tooltip).color).toBe(colors.color)
    await expect(getComputedStyle(tooltip).boxShadow).not.toBe('none')
    await expect(getComputedStyle(tooltip).maxWidth).toBe('320px')
    await userEvent.unhover(time)
    await waitFor(() => expect(canvas.queryByRole('tooltip')).toBeNull())
    await expect(time).not.toHaveAttribute('aria-describedby')
  }
  await expect(canvas.getByTestId('relative')).toHaveAccessibleName('Updated 3m ago')
  const unreported = canvas.getByTestId('unreported')
  await expect(unreported).toHaveTextContent('—')
  await expect(unreported).not.toHaveAttribute('datetime')
  await expect(unreported).not.toHaveAttribute('aria-description')
  await userEvent.hover(unreported)
  await expect(canvas.queryByRole('tooltip')).toBeNull()
  await expect(unreported).not.toHaveAttribute('aria-describedby')
  await userEvent.unhover(unreported)
  // Leave a tooltip visible for comparison screenshots.
  await userEvent.hover(canvas.getByTestId('relative'))
  await tooltipFor(canvas.getByTestId('relative'), exact)
}
export const AllStates: Story = { play: assertStates }
export const Light440: Story = { args: { scheme: 'light', width: 440 }, play: assertStates }
export const Dark1440: Story = { args: { scheme: 'dark', width: 1440 }, play: assertStates }
export const Light1440: Story = { args: { scheme: 'light', width: 1440 }, play: assertStates }
export const HostTheme: Story = { args: { scheme: 'light', host: true }, play: assertStates }
export const LiveTick: Story = { args: { live: true }, play: async context => {
  await assertStates(context)
  const canvas = within(context.canvasElement)
  await userEvent.unhover(canvas.getByTestId('relative'))
  await userEvent.click(canvas.getByRole('button', { name: 'Advance fake timer by 1 second' }))
  await expect(canvas.getByTestId('elapsed')).toHaveTextContent('3m 1s')
  await expect(canvas.getByTestId('relative')).toHaveTextContent('3m ago')
  await expect(canvas.getByTestId('elapsed')).toHaveAttribute('datetime', '2026-10-12T15:57:00.000Z')
} }
/** Browser tests install fake wall-clock timers before mounting this real system-clock consumer. */
export const SystemTick: Story = { args: { browserClock: true } }
/**
 * The default hover delay is still 400ms: the tooltip mounts no earlier than 400ms after the
 * pointer enters. The play waits on the mount itself, not on a timeout, so load only slows it.
 */
export const DefaultHoverDelay: Story = { args: { instantTooltips: false }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const time = canvas.getByTestId('relative')
  const mounted = new Promise<number>(resolve => {
    const observer = new MutationObserver(() => {
      if (!time.hasAttribute('aria-describedby')) return
      observer.disconnect()
      resolve(performance.now())
    })
    observer.observe(time, { attributes: true, attributeFilter: ['aria-describedby'] })
  })
  const start = performance.now()
  fireEvent.pointerOver(time)
  // 1ms below 400 absorbs performance.now() coarsening; the timer itself never fires early.
  await expect((await mounted) - start).toBeGreaterThanOrEqual(399)
  await tooltipFor(time, exact)
  fireEvent.pointerOut(time)
  await waitFor(() => expect(canvas.queryByRole('tooltip')).toBeNull())
  await expect(time).not.toHaveAttribute('aria-describedby')
} }
const styles = stylex.create({
  root: { minHeight: '100vh', boxSizing: 'border-box', padding: s.xl, backgroundColor: surface.canvas, color: text.fg, fontFamily: t.fontSans },
  width: (width: number) => ({ width, maxWidth: '100%' }),
  list: { display: 'grid', gridTemplateColumns: 'max-content 1fr', gap: s.md },
  colors: { backgroundColor: text.fg, color: surface.raised },
})

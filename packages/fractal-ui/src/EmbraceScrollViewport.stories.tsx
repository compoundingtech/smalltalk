import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { EmbraceScrollViewport } from './assistant-ui/EmbraceScrollViewport'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

function ScrollStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [count, setCount] = React.useState(40)
  const [narrow, setNarrow] = React.useState(false)
  const rows = React.useMemo(() => Array.from({ length: count }, (_, index) => ({ id: `entry-${index}`, text: `Observed message ${index + 1}: preserve the reader's position as the conversation grows or the viewport changes width.` })), [count])
  return <main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><div {...stylex.props(styles.actions)}><Button onPress={() => setCount(value => value + 1)} {...stylex.props(styles.button)}>Append message</Button><Button onPress={() => setNarrow(value => !value)} {...stylex.props(styles.button)}>Change width</Button></div><section {...stylex.props(styles.frame, narrow && styles.narrow)}><EmbraceScrollViewport items={rows} data-testid="scroll-viewport" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.viewport)} contentProps={stylex.props(styles.rows)}>{rows.map(row => <article key={row.id} data-item-id={row.id} {...stylex.props(styles.row)}>{row.text}</article>)}</EmbraceScrollViewport></section></main>
}
const meta = { title: 'Fractal UI/Scroll Viewport', component: ScrollStory, args: { scheme: 'dark' }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof ScrollStory>
export default meta
type Story = StoryObj<typeof meta>
const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
export const FollowingAndAnchored: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const viewport = canvas.getByTestId('scroll-viewport')
  await document.fonts.ready
  await frame()
  await expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(1)
  await userEvent.click(canvas.getByRole('button', { name: 'Append message' }))
  await frame()
  await expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(1)
  viewport.focus()
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -400, bubbles: true }))
  viewport.scrollTop = 240
  viewport.dispatchEvent(new Event('scroll'))
  await frame()
  const anchor = Array.from(viewport.querySelectorAll<HTMLElement>('[data-item-id]')).find(row => row.getBoundingClientRect().bottom > viewport.getBoundingClientRect().top)!
  const offset = anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top
  await userEvent.click(canvas.getByRole('button', { name: 'Append message' }))
  await frame()
  await expect(Math.abs(anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(1)
  await expect(canvas.getByRole('button', { name: 'Scroll to end' })).toBeVisible()
  await userEvent.click(canvas.getByRole('button', { name: 'Change width' }))
  await frame()
  await expect(Math.abs(anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(1)
  await userEvent.click(canvas.getByRole('button', { name: 'Scroll to end' }))
  await frame()
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(1))
  await expect(canvas.queryByRole('button', { name: 'Scroll to end' })).toBeNull()
} }
export const FollowingAndAnchoredLight: Story = { ...FollowingAndAnchored, args: { scheme: 'light' } }
export const AllStates: Story = { render: args => <ScrollStory {...args} /> }
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: '100vh', boxSizing: 'border-box', padding: s.lg, gap: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans }, actions: { display: 'flex', gap: s.md },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  frame: { display: 'flex', flex: '1 1 0', minHeight: 0, width: '100%' }, narrow: { width: '50%' }, viewport: { flex: '1 1 0', minHeight: 0, overflowY: 'auto', overflowAnchor: 'none' }, rows: { display: 'flex', flexDirection: 'column', gap: s.lg }, row: { padding: s.lg, minHeight: g.resourceCard, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, fontSize: t.bodySize, lineHeight: t.bodyLeading },
})

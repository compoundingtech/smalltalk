/** Show menu: multiple-selection checkbox entries with right-aligned tabular counts, in both themes at 440 and 1440. Synthetic hosts only. */
import * as React from 'react'
import type { Meta, StoryObj } from '@storybook/react-vite'
import * as stylex from '@stylexjs/stylex'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { ShowMenu, type ShowMenuGroup } from './SidebarShowMenu'
import { lightTheme as compositionLightTheme } from '../composition-theme'
import { borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

const groups: readonly ShowMenuGroup[] = [
  { id: 'agents', title: 'Agents', entries: [{ id: 'is:asking', label: 'Waiting for answer', count: 2 }] },
  { id: 'machines', title: 'Machine', entries: [
    { id: 'machine:host-a', label: 'host-a', count: 12 },
    { id: 'machine:host-b', label: 'host-b', count: 1 },
    { id: 'machine:host-with-a-long-synthetic-name', label: 'host-with-a-long-synthetic-name', count: 0 },
  ] },
]

function ShowMenuHarness({ dark = true, width = 1440 }: { readonly dark?: boolean; readonly width?: number }) {
  const [selected, setSelected] = React.useState<ReadonlySet<string>>(new Set())
  return <main data-scheme={dark ? 'dark' : 'light'} {...stylex.props(styles.canvas, width === 440 ? styles.narrow : styles.wide, !dark && compositionLightTheme)}>
    <div {...stylex.props(styles.bar)}>
      <span {...stylex.props(styles.search)}>Search agents</span>
      <ShowMenu groups={groups} selectedKeys={selected} onSelectionChange={setSelected} {...(dark ? {} : { popoverTheme: compositionLightTheme })} />
    </div>
    <output data-testid="show-selection" {...stylex.props(styles.output)}>{[...selected].join(' ')}</output>
  </main>
}

/** Tabular data rule: every count shares one right edge and uses tabular figures. */
async function assertCountsAligned(menu: HTMLElement) {
  const counts = [...menu.querySelectorAll<HTMLElement>('[data-show-count]')]
  await expect(counts).toHaveLength(4)
  const right = counts[0]!.getBoundingClientRect().right
  for (const count of counts) {
    await expect(Math.abs(count.getBoundingClientRect().right - right)).toBeLessThanOrEqual(1)
    await expect(getComputedStyle(count).fontVariantNumeric).toContain('tabular-nums')
  }
}
/** The custom menu is the only disclosure: no native title tooltips. */
async function assertNoNativeTitles(root: HTMLElement) {
  await expect(root.querySelector('[title]')).toBeNull()
}

const play: NonNullable<Story['play']> = async ({ canvasElement }) => {
  const body = within(canvasElement.ownerDocument.body)
  const trigger = body.getByRole('button', { name: 'Show' })
  await userEvent.click(trigger)
  const menu = await body.findByRole('menu', { name: 'Show' })
  const items = within(menu).getAllByRole('menuitemcheckbox')
  await expect(items.map(item => item.getAttribute('aria-label'))).toEqual(['Waiting for answer, 2', 'host-a, 12', 'host-b, 1', 'host-with-a-long-synthetic-name, 0'])
  for (const item of items) await expect(item).toHaveAttribute('aria-checked', 'false')
  await assertCountsAligned(menu)
  await assertNoNativeTitles(menu)
  await waitFor(() => {
    const bounds = menu.getBoundingClientRect()
    expect(bounds.left).toBeGreaterThanOrEqual(0)
    expect(bounds.right).toBeLessThanOrEqual(canvasElement.ownerDocument.documentElement.clientWidth)
  })

  // Multiple selection keeps the menu open; both entries apply together.
  await userEvent.click(items[1]!)
  await userEvent.click(items[0]!)
  await expect(menu).toBeInTheDocument()
  await expect(items[0]).toHaveAttribute('aria-checked', 'true')
  await expect(items[1]).toHaveAttribute('aria-checked', 'true')
  await expect(items[2]).toHaveAttribute('aria-checked', 'false')
  await expect(body.getByTestId('show-selection')).toHaveTextContent('machine:host-a is:asking')
  await expect(trigger).toHaveAccessibleName(/^Show\s*,\s*2 active$/)

  // Negative controls: a misaligned count and an injected native title must each be rejected.
  const count = menu.querySelector<HTMLElement>('[data-show-count]')!
  count.style.marginRight = '7px'
  await expect(assertCountsAligned(menu)).rejects.toThrow()
  count.style.marginRight = ''
  await assertCountsAligned(menu)
  items[2]!.setAttribute('title', 'injected')
  await expect(assertNoNativeTitles(menu)).rejects.toThrow()
  items[2]!.removeAttribute('title')
  await assertNoNativeTitles(menu)

  // Unchecking clears the entry; Escape closes and returns focus to the trigger.
  await userEvent.click(items[1]!)
  await expect(items[1]).toHaveAttribute('aria-checked', 'false')
  await expect(body.getByTestId('show-selection')).toHaveTextContent(/^is:asking$/)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(body.queryByRole('menu', { name: 'Show' })).toBeNull())
  await waitFor(() => expect(trigger).toHaveFocus())
  await expect(trigger).toHaveAccessibleName(/^Show\s*,\s*1 active$/)
}

const meta = {
  title: 'Fractal UI/Sidebar/Show Menu',
  component: ShowMenuHarness,
  parameters: {
    layout: 'fullscreen',
    viewport: { options: {
      show1440: { name: 'Show desktop', styles: { width: '1440px', height: '900px' }, type: 'desktop' },
      show440: { name: 'Show narrow', styles: { width: '440px', height: '900px' }, type: 'mobile' },
    } },
    a11y: { config: { rules: [{ id: 'label-content-name-mismatch', enabled: true }] } },
    docs: { description: { component: 'Sidebar Show menu: React Aria multiple-selection menu whose checkbox entries carry right-aligned tabular counts. Selection keys and their meaning belong to the host.' } },
  },
  globals: { viewport: { value: 'show1440', isRotated: false } },
  args: { dark: true, width: 1440 },
} satisfies Meta<typeof ShowMenuHarness>
export default meta
type Story = StoryObj<typeof meta>
export const Wide1440Dark: Story = { play }
export const Wide1440Light: Story = { args: { dark: false }, play }
export const Narrow440Dark: Story = { args: { width: 440 }, globals: { viewport: { value: 'show440', isRotated: false } }, play }
export const Narrow440Light: Story = { args: { width: 440, dark: false }, globals: { viewport: { value: 'show440', isRotated: false } }, play }

const styles = stylex.create({
  canvas: { minHeight: '100vh', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, display: 'flex', flexDirection: 'column', gap: s.lg },
  wide: { width: g.polishWide, maxWidth: '100%' },
  narrow: { width: g.polishNarrow, maxWidth: '100%' },
  bar: { display: 'flex', alignItems: 'stretch', gap: s.sm, width: g.sidebarDefault, maxWidth: '100%' },
  search: { flexGrow: 1, minWidth: 0, display: 'flex', alignItems: 'center', paddingInline: s.md, borderRadius: r.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, backgroundColor: surface.message, color: ink.fgMuted },
  output: { color: ink.fgMuted },
})

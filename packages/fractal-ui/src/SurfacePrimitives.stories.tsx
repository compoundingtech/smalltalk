import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import type { Selection } from 'react-aria-components'
import { Button, Checkbox, Menu, MenuItem, MenuTrigger, Popover, Tooltip } from './assistant-ui/composition/Controls'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { accentVars as accent, elevationVars as elevation, interactionVars as interaction, surfaceVars as surface, textVars as text, typeVars as type, spaceVars as space } from './assistant-ui/composition-tokens.stylex'

function SurfacePrimitivesStory({ scheme = 'dark', width = 440, selectionMode = 'multiple', shortcut = '⌘K' }: { scheme?: Scheme; width?: number; selectionMode?: 'single' | 'multiple'; shortcut?: string }) {
  const [revised, setRevised] = React.useState(false)
  const [selectedKeys, setSelectedKeys] = React.useState<Selection>(() => new Set())
  return <main data-scheme={scheme} data-testid="surface-primitives" {...stylex.props(styles.root, styles.width(width), ...baselineTheme, scheme === 'light' && lightTheme)}>
    <h1 {...stylex.props(styles.heading)}>Surface primitives</h1>
    <MenuTrigger><Button variant="outline">View options</Button><Popover data-testid="menu-surface"><Menu aria-label="View options" selectionMode={selectionMode} selectedKeys={selectedKeys} onSelectionChange={setSelectedKeys} shouldCloseOnSelect={false}>
      <MenuItem id="timestamps" textValue="Show timestamps">{revised ? 'Show event timestamps' : 'Show timestamps'}</MenuItem>
      <MenuItem id="details" textValue="Show details" shortcut="⌘D">Show details</MenuItem>
      <MenuItem id="unavailable" isDisabled>Host unavailable</MenuItem>
    </Menu></Popover></MenuTrigger>
    <MenuTrigger><Button variant="outline">Thread actions</Button><Popover><Menu aria-label="Thread actions">
      <MenuItem id="rename">Rename</MenuItem>
      <MenuItem id="archive" shortcut="⌘E">Archive</MenuItem>
    </Menu></Popover></MenuTrigger>
    <Checkbox>Wrap long lines</Checkbox>
    <Checkbox defaultSelected>Follow new events</Checkbox>
    <Checkbox isIndeterminate>Some threads selected</Checkbox>
    <Checkbox isDisabled>Archived thread</Checkbox>
    <Tooltip label={revised ? 'Search this thread' : 'Search messages'} shortcut={shortcut}><Button>Search</Button></Tooltip>
    <Tooltip label="Copy thread link"><Button>Copy link</Button></Tooltip>
    <Button onPress={() => setRevised(value => !value)}>Update labels</Button>
    <output aria-label="Label revision">{revised ? 'Updated' : 'Original'}</output>
    <span data-testid="surface-reference" aria-hidden="true" {...stylex.props(styles.reference)} />
    {/* LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback — DELETE at contraction — https://www.notion.so/3f7e3d41f4a381839fa5f7b257ba8f3c */}
    <span data-testid="bridge-ring" data-focus-visible="" aria-hidden="true" {...stylex.props(styles.probe)} />
    {/* LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback */}
  </main>
}
const meta = { title: 'Fractal UI/Surface primitives', component: SurfacePrimitivesStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', width: 440, selectionMode: 'multiple', shortcut: '⌘K' }, argTypes: { scheme: { options: ['light', 'dark'], control: 'radio' }, width: { options: [440, 1440], control: 'radio' }, selectionMode: { options: ['single', 'multiple'], control: 'radio' } } } satisfies Meta<typeof SurfacePrimitivesStory>
export default meta
type Story = StoryObj<typeof meta>

/** The probe exercises the real entry keyframes and also proves a resting surface does not animate. */
async function assertMotion(element: HTMLElement) {
  await waitFor(() => expect(element).not.toHaveAttribute('data-entering'))
  const view = element.ownerDocument.defaultView!
  await expect(view.getComputedStyle(element).animationName).toBe('none')
  element.setAttribute('data-entering', '')
  try {
    const computed = view.getComputedStyle(element)
    if (view.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      await expect(computed.animationName).toBe('none')
      await expect(parseFloat(computed.animationDuration)).toBe(0)
      await expect(computed.transform).toBe('none')
    } else {
      await expect(computed.animationName).not.toBe('none')
      await expect(parseFloat(computed.animationDuration)).toBe(0.15)
      const animation = element.getAnimations()[0]
      await expect(animation).toBeDefined()
      const frames = (animation!.effect as KeyframeEffect).getKeyframes()
      await expect(Number(frames[0]!.opacity)).toBe(0)
      await expect(String(frames[0]!.transform)).toContain('0.98')
      await expect(Number(frames[frames.length - 1]!.opacity)).toBe(1)
    }
  } finally {
    element.removeAttribute('data-entering')
  }
}
/** TooltipSurface deliberately has no entry animation, even when RAC marks it entering. */
async function assertNoMotion(element: HTMLElement) {
  await expect(getComputedStyle(element).animationName).toBe('none')
  element.setAttribute('data-entering', '')
  try {
    await expect(getComputedStyle(element).animationName).toBe('none')
  } finally {
    element.removeAttribute('data-entering')
  }
}
type Rgba = readonly [number, number, number, number]
const parseColor = (value: string): Rgba => {
  const [r = 0, g = 0, b = 0, a = 1] = value.match(/[\d.]+/g)!.map(Number)
  return [r, g, b, a]
}
const composite = ([r, g, b, a]: Rgba, [br, bg, bb]: Rgba): Rgba => [r * a + br * (1 - a), g * a + bg * (1 - a), b * a + bb * (1 - a), 1]
const luminance = (color: Rgba) => {
  const [r = 0, g = 0, b = 0] = color.slice(0, 3).map(channel => { const value = channel / 255; return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4 })
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}
/** WCAG contrast of `ink` over computed layers listed bottom-up; translucent layers blend onto the opaque surface below. */
function contrast(ink: string, ...layers: [string, ...string[]]) {
  const background = layers.map(parseColor).reduce((below, layer) => composite(layer, below))
  const [high, low] = [luminance(composite(parseColor(ink), background)), luminance(background)].sort((a, b) => b - a) as [number, number]
  return (high + 0.05) / (low + 0.05)
}
/** Inline labels use inherited leading; text fragments reveal wrapping without assuming a block line box. */
function labelLineCount(label: HTMLElement) {
  const range = label.ownerDocument.createRange()
  range.selectNodeContents(label)
  return range.getClientRects().length
}
async function assertOneLine(label: HTMLElement) {
  await expect(labelLineCount(label)).toBe(1)
}
const exercise: NonNullable<Story['play']> = async ({ canvasElement, args }) => {
  const canvas = within(canvasElement)
  const root = canvas.getByTestId('surface-primitives')
  await expect(root).toHaveAttribute('data-scheme', args.scheme)
  await expect(root.getBoundingClientRect().width).toBeLessThanOrEqual(args.width!)
  const reference = getComputedStyle(canvas.getByTestId('surface-reference'))
  // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback — DELETE at contraction — https://www.notion.so/3f7e3d41f4a381839fa5f7b257ba8f3c
  // The tokens.css fallback ring for app controls resolves to the same scheme-aware token.
  const bridgeRing = getComputedStyle(canvas.getByTestId('bridge-ring'))
  await expect(bridgeRing.outlineStyle).toBe('solid')
  await expect(bridgeRing.outlineWidth).toBe('2px')
  await expect(bridgeRing.outlineColor).toBe(reference.outlineColor)
  // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback
  const checkbox = canvas.getByRole('checkbox', { name: 'Wrap long lines' })
  const mark = checkbox.closest('label')!.querySelector<HTMLElement>('span[aria-hidden="true"]')!
  await expect(checkbox).not.toBeChecked()
  await expect(mark.querySelector('svg')).toBeNull()
  await expect(mark.getBoundingClientRect().width).toBe(14)
  await expect(mark.getBoundingClientRect().height).toBe(14)
  await userEvent.click(checkbox)
  await expect(checkbox).toBeChecked()
  await expect(mark.querySelector('svg')).not.toBeNull()
  await expect(getComputedStyle(mark).backgroundColor).toBe(reference.color)
  await expect(checkbox.closest('label')!.offsetHeight).toBe(24)
  await expect(canvas.getByRole('checkbox', { name: 'Archived thread' })).toBeDisabled()

  // An action menu without selection or glyphs reserves no leading slot.
  const actions = canvas.getByRole('button', { name: 'Thread actions' })
  actions.focus()
  await userEvent.keyboard('{ArrowDown}')
  const rename = await canvas.findByRole('menuitem', { name: 'Rename' })
  await expect(rename).toHaveFocus()
  await expect(rename.querySelector('span[aria-hidden="true"]')).toBeNull()
  await expect(getComputedStyle(rename).backgroundColor).toBe(reference.backgroundColor)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('menu')).toBeNull())
  await waitFor(() => expect(actions).toHaveFocus())

  const trigger = canvas.getByRole('button', { name: 'View options' })
  trigger.focus()
  await userEvent.keyboard('{ArrowDown}')
  const role = args.selectionMode === 'single' ? 'menuitemradio' : 'menuitemcheckbox'
  const first = await canvas.findByRole(role, { name: /Show timestamps/ })
  await expect(first).toHaveFocus()
  await expect(first).toHaveAttribute('aria-checked', 'false')
  await expect(first.querySelector('svg')).toBeNull()
  const slot = first.querySelector<HTMLElement>('span[aria-hidden="true"]')!
  // Measure after the entry scale settles; mid-animation boxes report the transformed width.
  await waitFor(() => expect(canvas.getByTestId('menu-surface')).not.toHaveAttribute('data-entering'))
  await expect(slot.getBoundingClientRect().width).toBe(16)
  const surfaceStyle = getComputedStyle(canvas.getByTestId('menu-surface'))
  await expect(surfaceStyle.borderTopLeftRadius).toBe('8px')
  await expect(surfaceStyle.paddingTop).toBe('4px')
  await expect(surfaceStyle.paddingLeft).toBe('4px')
  await expect(surfaceStyle.borderTopWidth).toBe('1px')
  await expect(surfaceStyle.boxShadow).toBe(reference.boxShadow)
  await expect(getComputedStyle(first).borderTopLeftRadius).toBe('6px')
  await expect(getComputedStyle(first).backgroundColor).toBe(reference.backgroundColor)
  await expect(getComputedStyle(first).outlineColor).toBe(reference.outlineColor)
  await userEvent.keyboard(' ')
  await expect(first).toHaveAttribute('aria-checked', 'true')
  await expect(first.querySelector('svg')).not.toBeNull()
  await expect(parseFloat(getComputedStyle(first).height)).toBe(28)
  await expect(parseFloat(getComputedStyle(first).fontSize)).toBe(14)
  await expect(parseFloat(getComputedStyle(first).lineHeight)).toBe(20)
  await expect(parseFloat(getComputedStyle(first).outlineWidth)).toBe(2)
  await expect(parseFloat(getComputedStyle(first).outlineOffset)).toBe(-2)
  await assertMotion(canvas.getByTestId('menu-surface'))
  const second = canvas.getByRole(role, { name: /Show details/ })
  const shortcut = second.querySelector('kbd')!
  await expect(second).not.toHaveAttribute('data-hovered')
  const restInk = getComputedStyle(shortcut).color
  await userEvent.keyboard('{ArrowDown}')
  await expect(second).toHaveFocus()
  // Contrast is read after the focus re-render lifts the shortcut ink on the wash.
  await waitFor(() => expect(getComputedStyle(shortcut).color).not.toBe(restInk))
  const surfaceFill = getComputedStyle(canvas.getByTestId('menu-surface')).backgroundColor
  const washFill = getComputedStyle(second).backgroundColor
  await expect(washFill).toBe(reference.backgroundColor)
  const activeInk = getComputedStyle(shortcut).color
  await expect(contrast(activeInk, surfaceFill, washFill)).toBeGreaterThanOrEqual(4.5)
  await expect(contrast(restInk, surfaceFill)).toBeGreaterThanOrEqual(4.5)
  // Negative controls: identical ink reads 1:1, and the resting muted ink on the dark wash is below AA, so the ratio check can fail.
  await expect(contrast(surfaceFill, surfaceFill)).toBe(1)
  if (args.scheme === 'dark') await expect(contrast(restInk, surfaceFill, washFill)).toBeLessThan(4.5)
  await userEvent.keyboard(' ')
  await expect(second).toHaveAttribute('aria-checked', 'true')
  await expect(first).toHaveAttribute('aria-checked', args.selectionMode === 'single' ? 'false' : 'true')
  await userEvent.keyboard('{ArrowUp}')
  await waitFor(() => expect(getComputedStyle(shortcut).color).toBe(restInk))
  // Restore the first selection in radio mode so the same rerender assertion covers both modes.
  if (args.selectionMode === 'single') await userEvent.keyboard(' ')
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('menu')).toBeNull())
  await waitFor(() => expect(trigger).toHaveFocus())

  // Local state rerenders the production children; selection and tooltip copy must survive it.
  await userEvent.click(canvas.getByRole('button', { name: 'Update labels' }))
  await expect(canvas.getByRole('status', { name: 'Label revision' })).toHaveTextContent('Updated')
  trigger.focus()
  await userEvent.keyboard('{ArrowDown}')
  const updated = await canvas.findByRole(role, { name: /Show event timestamps/ })
  await expect(updated).toHaveAttribute('aria-checked', 'true')
  await expect(updated.querySelector('svg')).not.toBeNull()
  await expect(canvas.getByTestId('menu-surface').closest('[data-scheme]')).toBe(root)
  await assertMotion(canvas.getByTestId('menu-surface'))
  // The revised label is wider than the menu minimum; the surface grows to its rows instead of wrapping them.
  const menuSurface = canvas.getByTestId('menu-surface')
  const updatedLabel = [...updated.querySelectorAll('span')].find(span => span.textContent === 'Show event timestamps')!
  await expect(menuSurface.offsetWidth).toBeLessThanOrEqual(320)
  await assertOneLine(updatedLabel)
  await expect(updatedLabel.scrollWidth).toBeLessThanOrEqual(updatedLabel.clientWidth)
  for (const item of within(canvas.getByRole('menu')).getAllByRole(role)) {
    await expect(item.offsetHeight).toBe(28)
    await expect(item.scrollHeight).toBeLessThanOrEqual(item.clientHeight)
  }
  // Negative control: a squeezed menu truncates the label on one line and keeps the 28px row.
  const menuList = canvas.getByRole('menu')
  menuList.style.minWidth = '0px'
  menuList.style.width = '160px'
  try {
    await expect(updatedLabel.scrollWidth).toBeGreaterThan(updatedLabel.clientWidth)
    await assertOneLine(updatedLabel)
    await expect(updated.offsetHeight).toBe(28)
  } finally {
    menuList.style.minWidth = ''
    menuList.style.width = ''
  }
  await expect(updatedLabel.scrollWidth).toBeLessThanOrEqual(updatedLabel.clientWidth)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('menu')).toBeNull())
  await waitFor(() => expect(trigger).toHaveFocus())
  await expect(checkbox).toBeChecked()

  // Hover runs before any other tooltip opens, so React Aria's warm-up cannot skip the 350ms delay.
  // The menu steps leave React Aria in keyboard modality, where hover never opens a tooltip; a pointer press restores pointer modality.
  await userEvent.click(canvas.getByRole('heading', { name: 'Surface primitives' }))
  const copy = canvas.getByRole('button', { name: 'Copy link' })
  const hoverStart = performance.now()
  await userEvent.hover(copy)
  await expect(canvas.queryByRole('tooltip')).toBeNull()
  const plain = await canvas.findByRole('tooltip', {}, { timeout: 2000 })
  await expect(performance.now() - hoverStart).toBeGreaterThanOrEqual(350)
  await expect(plain).toHaveTextContent('Copy thread link')
  await expect(plain.querySelector('kbd')).toBeNull()
  await assertNoMotion(plain)
  const plainLabel = plain.querySelector('span')!
  await assertOneLine(plainLabel)
  const plainHeight = plain.offsetHeight
  // Negative control: a bound narrower than the label wraps it, so the one-line check can fail.
  plain.style.maxWidth = '48px'
  try {
    await expect(labelLineCount(plainLabel)).toBeGreaterThan(1)
    await expect(plain.offsetHeight).toBeGreaterThan(plainHeight)
  } finally {
    plain.style.maxWidth = ''
  }
  await assertOneLine(plainLabel)
  await expect(plain.offsetHeight).toBe(plainHeight)
  await userEvent.unhover(copy)
  await waitFor(() => expect(canvas.queryByRole('tooltip')).toBeNull())

  const search = canvas.getByRole('button', { name: 'Search' })
  // Focus opens the tooltip only while focus is visible; a non-modifier key returns to keyboard modality (modifiers are ignored).
  await userEvent.keyboard('{Escape}')
  search.focus()
  const tooltip = await canvas.findByRole('tooltip')
  await expect(tooltip).toHaveTextContent('Search this thread')
  await expect(tooltip).not.toHaveTextContent('Search messages')
  await expect(search).toHaveAccessibleDescription(`Search this thread ${args.shortcut}`)
  await expect(within(tooltip).getByText(args.shortcut!)).toBeVisible()
  await expect(tooltip.closest('[data-scheme]')).toBe(root)
  await assertNoMotion(tooltip)
  const tooltipLabel = tooltip.querySelector('span')!
  await assertOneLine(tooltipLabel)
  await expect(tooltip.scrollHeight).toBe(tooltip.clientHeight)
  await expect(tooltip.offsetWidth).toBeLessThanOrEqual(320)
  const tooltipStyle = getComputedStyle(tooltip)
  await expect(parseFloat(tooltipStyle.fontSize)).toBe(11)
  await expect(tooltipStyle.borderTopLeftRadius).toBe('8px')
  await expect(tooltipStyle.paddingTop).toBe('6px')
  await expect(tooltipStyle.paddingLeft).toBe('10px')
  await expect(tooltipStyle.borderTopWidth).toBe('0px')
  // Height follows inherited leading and the shortcut, not the discarded fixed 26px surface.
  const tooltipHeight = tooltip.offsetHeight
  tooltip.style.maxWidth = '48px'
  try {
    await expect(labelLineCount(tooltipLabel)).toBeGreaterThan(1)
    await expect(tooltip.offsetHeight).toBeGreaterThan(tooltipHeight)
  } finally {
    tooltip.style.maxWidth = ''
  }
  await assertOneLine(tooltipLabel)
  await expect(tooltip.offsetHeight).toBe(tooltipHeight)
  await expect(Math.round(tooltip.getBoundingClientRect().top - search.getBoundingClientRect().bottom)).toBe(4)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('tooltip')).toBeNull())
  search.blur()
  await userEvent.click(trigger)
  const finalSelection = await canvas.findByRole(role, { name: /Show event timestamps/ })
  await expect(finalSelection).toHaveAttribute('aria-checked', 'true')
  await expect(finalSelection.querySelector('svg')).not.toBeNull()
  // Pointer hover lifts the shortcut ink exactly as keyboard focus does.
  const hovered = canvas.getByRole(role, { name: /Show details/ })
  await userEvent.hover(hovered)
  await waitFor(() => expect(hovered).toHaveAttribute('data-hovered'))
  await waitFor(() => expect(getComputedStyle(hovered.querySelector('kbd')!).color).toBe(activeInk))
}
export const Dark440: Story = { play: exercise }
export const Light440: Story = { args: { scheme: 'light' }, play: exercise }
export const Dark1440: Story = { args: { width: 1440 }, play: exercise }
export const Light1440: Story = { args: { scheme: 'light', width: 1440 }, play: exercise }
export const RadioSelection: Story = { args: { selectionMode: 'single', shortcut: 'Ctrl+K' }, play: exercise }
export const AllStates: Story = { args: { scheme: 'light', width: 1440, shortcut: 'Ctrl+K' }, play: exercise }
const styles = stylex.create({
  root: { minHeight: '100vh', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: space.lg, padding: space.xl, color: text.fg, backgroundColor: surface.canvas, fontFamily: type.fontSans },
  width: (width: number) => ({ width, maxWidth: '100%' }),
  heading: { margin: 0, fontSize: type.headingSize, lineHeight: type.headingLeading, fontWeight: type.weightMedium },
  reference: { position: 'absolute', visibility: 'hidden', color: accent.primary, backgroundColor: interaction.wash, outlineColor: interaction.focusRing, boxShadow: elevation.popover },
  // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback — DELETE at contraction — https://www.notion.so/3f7e3d41f4a381839fa5f7b257ba8f3c
  probe: { position: 'absolute', visibility: 'hidden' },
  // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback
})

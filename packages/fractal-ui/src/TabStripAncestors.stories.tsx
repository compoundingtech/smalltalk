import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { TabPanel, Tabs } from 'react-aria-components'
import { Button } from './assistant-ui/composition/Controls'
import { TabStrip, type TabStripItem } from './assistant-ui/workbench/TabStrip'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface } from './assistant-ui/composition-tokens.stylex'

const items: readonly TabStripItem[] = [
  { key: 'coordinator', label: 'Release coordinator', icon: 'message', kind: 'thread' },
  { key: 'review', label: 'Accessibility review', icon: 'message', kind: 'thread' },
  { key: 'diff', label: 'Navigation changes', icon: 'pull-request', kind: 'diff' },
  { key: 'shell', label: 'Build shell', icon: 'drawer', kind: 'terminal' },
  { key: 'notes', label: 'Release notes', icon: 'copy', kind: 'document' },
]
const width = 640
// The themed surface spans the 640px fixture even in a narrower viewport, so overflowing tabs never paint over the bare page.
const styles = stylex.create({ root: { backgroundColor: surface.canvas, width: 'max-content', minWidth: '100%' } })
interface Args extends React.ComponentProps<typeof TabStrip> { scenario: 'resize' | 'scroll'; scheme: 'dark' | 'light' }

const meta = {
  title: 'Fractal UI/Tab Strip Ancestors',
  component: TabStrip,
  parameters: { layout: 'fullscreen' },
  args: { items, selectedKey: 'coordinator', scenario: 'resize', scheme: 'dark' },
  render: function Render({ scenario, scheme, ...args }) {
    const ancestor = React.useRef<HTMLDivElement>(null)
    // Only the clipping wrapper geometry is story-owned; RAC and TabStrip are production mounts.
    return <section {...stylex.props(styles.root, ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}>
      <Button onPress={() => {
        const host = ancestor.current!
        if (scenario === 'resize') host.style.width = `${width / 2}px`
        else host.scrollLeft = width / 10
      }}>Change ancestor</Button>
      <Button onPress={() => {
        const host = ancestor.current!
        host.style.width = `${width}px`
        host.scrollLeft = 0
      }}>Restore ancestor</Button>
      <div ref={ancestor} data-testid="clipping-ancestor" style={{ width, overflow: scenario === 'resize' ? 'hidden' : 'auto', scrollbarWidth: 'none' }}>
        <div style={{ width: scenario === 'scroll' ? width + 200 : width }}>
          {/* Each tab controls a mounted panel, as in Workbench; RAC mounts only the selected one. */}
          <Tabs defaultSelectedKey={args.selectedKey} style={{ width }}><TabStrip {...args} />{args.items.map(item => <TabPanel key={item.key} id={item.key} />)}</Tabs>
        </div>
      </div>
    </section>
  },
} satisfies Meta<Args>
export default meta
type Story = StoryObj<typeof meta>

function requireContract(condition: boolean, reason: string): asserts condition {
  if (!condition) throw new Error(reason)
}
async function frames() {
  for (let index = 0; index < 3; index++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
}
type Box = { left: number; right: number; top: number; bottom: number }
function clipFor(strip: HTMLElement): Box {
  const clip: Box = { left: -Infinity, right: Infinity, top: -Infinity, bottom: Infinity }
  // Independent oracle uses actual browser padding boxes, never layout policy or clipping datasets.
  for (let element: HTMLElement | null = strip; element; element = element.parentElement) {
    const css = getComputedStyle(element)
    const rect = element.getBoundingClientRect()
    if (/^(auto|scroll|hidden|clip)$/.test(css.overflowX)) {
      clip.left = Math.max(clip.left, rect.left + element.clientLeft)
      clip.right = Math.min(clip.right, rect.left + element.clientLeft + element.clientWidth)
    }
    if (/^(auto|scroll|hidden|clip)$/.test(css.overflowY)) {
      clip.top = Math.max(clip.top, rect.top + element.clientTop)
      clip.bottom = Math.min(clip.bottom, rect.top + element.clientTop + element.clientHeight)
    }
  }
  return clip
}
function whole(box: Box, clip: Box) {
  return box.left >= clip.left && box.right <= clip.right && box.top >= clip.top && box.bottom <= clip.bottom
}
function painted(element: HTMLElement) {
  for (let current: HTMLElement | null = element; current; current = current.parentElement) {
    const css = getComputedStyle(current)
    if (css.display === 'none' || css.visibility === 'hidden' || Number(css.opacity) === 0) return false
  }
  return true
}
function assertVisibility(strip: HTMLElement, expected: readonly string[]) {
  const clip = clipFor(strip)
  const geometricallyWhole: string[] = []
  const visiblyPainted: string[] = []
  for (const tab of strip.querySelectorAll<HTMLElement>('[role="tab"]')) {
    const identity = tab.getAttribute('aria-label')!
    const contained = whole(tab.getBoundingClientRect(), clip)
    if (contained) geometricallyWhole.push(identity)
    requireContract(painted(tab) === contained, 'Tab paint disagrees with ancestor geometry')
    if (!painted(tab)) {
      requireContract(getComputedStyle(tab).pointerEvents === 'none', 'Clipped tab retains pointer interaction')
      continue
    }
    visiblyPainted.push(identity)
    const label = tab.querySelector<HTMLElement>('[data-tab-label]')!
    const icon = tab.querySelector<HTMLElement>('[data-tab-icon]')!
    const active = painted(label) ? label : icon
    requireContract(painted(active), 'Whole tab has no painted representation')
    requireContract(whole(active.getBoundingClientRect(), clip), 'Partial representation paint')
    if (active === icon) for (const graphic of icon.querySelectorAll<SVGGraphicsElement>('path, line, polyline, polygon, circle, ellipse, rect')) {
      const matrix = graphic.getScreenCTM()
      requireContract(matrix !== null, 'Missing painted SVG transform')
      const box = graphic.getBBox()
      const css = getComputedStyle(graphic)
      const stroke = css.stroke === 'none' ? 0 : parseFloat(css.strokeWidth) / 2
      const points = [[box.x - stroke, box.y - stroke], [box.x + box.width + stroke, box.y - stroke], [box.x - stroke, box.y + box.height + stroke], [box.x + box.width + stroke, box.y + box.height + stroke]].map(([x, y]) => new DOMPoint(x, y).matrixTransform(matrix))
      requireContract(whole({ left: Math.min(...points.map(point => point.x)), right: Math.max(...points.map(point => point.x)), top: Math.min(...points.map(point => point.y)), bottom: Math.max(...points.map(point => point.y)) }, clip), 'Partial representation paint')
    }
  }
  requireContract(JSON.stringify(geometricallyWhole) === JSON.stringify(expected), 'Unexpected whole-cell identities')
  requireContract(JSON.stringify(visiblyPainted) === JSON.stringify(expected), 'Unexpected painted identities')
}
async function rejectsStale(assertion: () => void) {
  let rejected = false
  try { assertion() } catch (error) {
    requireContract(error instanceof Error && error.message === 'Tab paint disagrees with ancestor geometry', `Unexpected control failure: ${String(error)}`)
    rejected = true
  }
  requireContract(rejected, 'Stale visibility control was not rejected')
}
async function fixture(canvasElement: HTMLElement) {
  await canvasElement.ownerDocument.fonts.ready
  await frames()
  const ancestor = canvasElement.querySelector<HTMLElement>('[data-testid="clipping-ancestor"]')!
  const strip = ancestor.querySelector<HTMLElement>('[data-tab-strip]')!
  requireContract(Boolean(ancestor && strip), 'Missing production strip fixture')
  assertVisibility(strip, items.map(item => item.label))
  const original = strip.getBoundingClientRect()
  const offset = strip.scrollLeft
  const unchanged = () => {
    const current = strip.getBoundingClientRect()
    requireContract(current.width === original.width && current.height === original.height, 'Ancestor change resized strip')
    requireContract(strip.scrollLeft === offset, 'Ancestor change scrolled TabList')
  }
  return { ancestor, strip, unchanged }
}

export const AncestorResize: Story = {
  play: async ({ canvasElement }) => {
    const { ancestor, strip, unchanged } = await fixture(canvasElement)
    const tabs = [...strip.querySelectorAll<HTMLElement>('[role="tab"]')]
    const oldPaint = tabs.map(tab => getComputedStyle(tab).opacity)
    const third = tabs[2].getBoundingClientRect()
    const narrowWidth = third.left + third.width / 2 - ancestor.getBoundingClientRect().left
    try {
      ancestor.style.width = `${narrowWidth}px`
      await frames()
      unchanged()
      assertVisibility(strip, items.slice(0, 2).map(item => item.label))
      // Reintroduce actual stale paint (not a dataset mutation) from before the ancestor shrank.
      for (const [index, tab] of tabs.entries()) tab.style.opacity = oldPaint[index]
      try { await rejectsStale(() => assertVisibility(strip, items.slice(0, 2).map(item => item.label))) }
      finally { for (const tab of tabs) tab.style.removeProperty('opacity') }
      assertVisibility(strip, items.slice(0, 2).map(item => item.label))
    } finally { ancestor.style.width = `${width}px` }
    await frames()
    unchanged()
    assertVisibility(strip, items.map(item => item.label))
  },
}

export const AncestorScroll: Story = {
  args: { scenario: 'scroll' },
  play: async ({ canvasElement }) => {
    const { ancestor, strip, unchanged } = await fixture(canvasElement)
    const first = strip.querySelector<HTMLElement>('[role="tab"]')!.getBoundingClientRect()
    const shift = Math.floor(first.width / 2)
    const ancestorBox = ancestor.getBoundingClientRect()
    const blockNotification = (event: Event) => event.stopImmediatePropagation()
    ancestor.addEventListener('scroll', blockNotification, { capture: true })
    try {
      // Prevent the production ancestor listener from receiving this real scroll.
      ancestor.scrollLeft = shift
      await frames()
      unchanged()
      requireContract(ancestor.scrollLeft === shift, 'Ancestor did not scroll')
      requireContract(strip.querySelector<HTMLElement>('[role="tab"]')!.getBoundingClientRect().left === first.left - shift, 'Ancestor scroll did not shift strip paint')
      requireContract(ancestor.getBoundingClientRect().width === ancestorBox.width && ancestor.getBoundingClientRect().height === ancestorBox.height, 'Scroll changed ancestor dimensions')
      await rejectsStale(() => assertVisibility(strip, items.slice(1).map(item => item.label)))
    } finally { ancestor.removeEventListener('scroll', blockNotification, { capture: true }) }
    try {
      // Deliver the missed notification, then exercise a second native ancestor scroll.
      ancestor.dispatchEvent(new Event('scroll'))
      assertVisibility(strip, items.slice(1).map(item => item.label))
      ancestor.scrollLeft = shift + 1
      await frames()
      unchanged()
      assertVisibility(strip, items.slice(1).map(item => item.label))
    } finally { ancestor.scrollLeft = 0 }
    await frames()
    unchanged()
    assertVisibility(strip, items.map(item => item.label))
  },
}

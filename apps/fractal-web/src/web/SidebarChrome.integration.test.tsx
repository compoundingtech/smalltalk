// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { within } from 'storybook/test'
import { ResponsiveAgentSidebar } from './ResponsiveAgentSidebar.tsx'
import { SidebarChromeFooter, SidebarConnectedStatus } from './SidebarChromeFooter.tsx'
import { SidebarToolbar } from '../shell/sidebar/Toolbar.tsx'
import { ResizableSplit } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { defaultFilters } from '../shell/sidebar/state.ts'

// Node cannot compile StyleX. Keep its authored scalar CSS, with RAC and chrome real.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'animation',
  props: (...styles: unknown[]) => {
    const merged = Object.assign({}, ...styles.filter(style => typeof style === 'object' && style !== null))
    return { style: Object.fromEntries(Object.entries(merged).filter(([key, value]) => !key.startsWith(':') && !key.startsWith('@') && (typeof value === 'string' || typeof value === 'number'))) }
  },
}))

let host: HTMLDivElement
let root: Root
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})
afterEach(async () => {
  await act(async () => root.unmount())
  host.remove()
  vi.unstubAllGlobals()
})

const ChromeFixture = ({ width, onCollapse, onTheme, connected = true }: {
  readonly width: number
  readonly onCollapse: () => void
  readonly onTheme: () => void
  readonly connected?: boolean
}) => {
  const [filters, setFilters] = React.useState(defaultFilters)
  return <ResponsiveAgentSidebar viewport={1440} width={width} maxWidth={440} collapsed={false} drawerOpen={false} onDrawerOpenChange={() => undefined} onToggleCollapsed={onCollapse} onWidthChange={() => undefined} onWidthCommit={() => undefined} onWidthReset={() => undefined}>
    <SidebarToolbar filters={filters} update={setFilters} hosts={['local']} />
    <SidebarChromeFooter onToggleScheme={onTheme}>{connected ? <SidebarConnectedStatus /> : null}</SidebarChromeFooter>
  </ResponsiveAgentSidebar>
}

describe.each([240, 256, 320, 440])('sidebar chrome at %ipx', width => {
  it('preserves named, focusable collapse and theme buttons with decorative stroke icons', async () => {
    const collapse = vi.fn(), theme = vi.fn()
    await act(async () => root.render(<ChromeFixture width={width} onCollapse={collapse} onTheme={theme} />))
    const sidebar = within(host).getByRole('complementary', { name: 'Agents' })
    for (const [name, callback] of [['Collapse agents', collapse], ['Toggle color scheme', theme]] as const) {
      const button = within(sidebar).getByRole('button', { name })
      expect(button.getAttribute('disabled')).toBeNull()
      button.focus()
      expect(document.activeElement).toBe(button)
      await act(async () => button.click())
      expect(callback).toHaveBeenCalledOnce()
      const svg = button.querySelector('svg')!
      expect(svg.getAttribute('aria-hidden')).toBe('true')
      expect(svg.getAttribute('fill')).toBe('none')
      expect(svg.getAttribute('stroke')).toBe('currentColor')
      expect(button.textContent).not.toMatch(/[«☀☾◌]/u)
    }
    expect(sidebar.querySelector<HTMLElement>('header strong')!.style.fontWeight).toBe('500')
    expect(within(sidebar).getByRole('status').textContent?.trim()).toBe('Connected')
    const dot = sidebar.querySelector<HTMLElement>('[data-sidebar-connection-dot]')!
    expect(dot.style.width).toBe('8px')
    expect(dot.style.height).toBe('8px')
    expect(dot.getAttribute('aria-hidden')).toBe('true')
  })

  it('uses equal 8px search insets and border-box sizing at every supported dock width', async () => {
    await act(async () => root.render(<ChromeFixture width={width} onCollapse={() => undefined} onTheme={() => undefined} />))
    const field = within(host).getByRole('textbox', { name: 'Search agents and folders' })
    const toolbar = field.parentElement!.parentElement!
    expect(toolbar.style.paddingInline).toBe('8px')
    expect(field.style.width).toBe('100%')
    expect(field.style.boxSizing).toBe('border-box')
    expect(field.style.borderRadius).toBe('6px')
    expect(within(host).getByRole('button', { name: 'Needs me only' })).not.toBeNull()
    expect(within(host).getByRole('button', { name: /Filter agents/ })).not.toBeNull()
    expect(within(host).getByRole('button', { name: /Sort agents/ })).not.toBeNull()
    field.focus()
    expect(document.activeElement).toBe(field)
  })
})

it('chrome footer adds no status or action of its own beyond the theme toggle', async () => {
  await act(async () => root.render(<ChromeFixture width={256} connected={false} onCollapse={() => undefined} onTheme={() => undefined} />))
  const footer = host.querySelector('footer')!
  expect(within(footer).queryByRole('status')).toBeNull()
  expect(within(footer).getAllByRole('button')).toHaveLength(1)
  expect(within(footer).queryByRole('button', { name: /Reconnect/ })).toBeNull()
})

describe('sidebar split (real kit ResizableSplit)', () => {
  const SplitFixture = ({ min = 160, max = 480, reverse, onChange, onCommit }: {
    readonly min?: number; readonly max?: number; readonly reverse?: boolean
    readonly onChange: (value: number) => void; readonly onCommit: (value: number) => void
  }) => {
    const [value, setValue] = React.useState(200)
    return <ResizableSplit id="sidebar-split-test" value={value} min={min} max={max} reverse={reverse} collapsed={false} label="Resize sidebar"
      onChange={next => { onChange(next); setValue(next) }} onCommit={onCommit} onToggle={() => undefined} onReset={() => undefined} />
  }
  const mountSplit = async (props: Omit<React.ComponentProps<typeof SplitFixture>, 'onChange' | 'onCommit'> = {}) => {
    const changes: number[] = []
    const commits: number[] = []
    await act(async () => root.render(<SplitFixture {...props} onChange={value => changes.push(value)} onCommit={value => commits.push(value)} />))
    return { changes, commits, separator: () => within(host).getByRole('separator', { name: 'Resize sidebar' }) }
  }
  const key = async (target: HTMLElement, value: string, shiftKey = false) => {
    await act(async () => { target.dispatchEvent(new KeyboardEvent('keydown', { key: value, shiftKey, bubbles: true, cancelable: true })) })
  }
  // jsdom lacks pointer capture and paints no frames; stub only those platform APIs.
  const pointer = async (target: HTMLElement, type: string, clientX: number) => {
    await act(async () => { target.dispatchEvent(new MouseEvent(type, { clientX, button: 0, bubbles: true, cancelable: true })) })
  }
  beforeEach(() => {
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { callback(0); return 1 })
    vi.stubGlobal('cancelAnimationFrame', () => undefined)
    Object.assign(HTMLElement.prototype, { setPointerCapture: () => undefined, releasePointerCapture: () => undefined, hasPointerCapture: () => false })
  })

  it('arrows grow toward the content in a normal layout; Home and End reach the declared bounds', async () => {
    const split = await mountSplit()
    const separator = split.separator()
    expect(separator.getAttribute('aria-orientation')).toBe('vertical')
    await key(separator, 'ArrowRight')
    await key(separator, 'ArrowRight', true)
    await key(separator, 'ArrowLeft')
    expect(split.commits).toEqual([208, 240, 232])
    expect(separator.getAttribute('aria-valuenow')).toBe('232')
    await key(separator, 'Home')
    await key(separator, 'End')
    expect(split.commits.slice(-2)).toEqual([160, 480])
  })

  it('reverses keyboard and pointer direction together for a trailing panel', async () => {
    const split = await mountSplit({ reverse: true })
    const separator = split.separator()
    await key(separator, 'ArrowRight')
    await key(separator, 'ArrowLeft', true)
    expect(split.commits).toEqual([192, 224])
    await pointer(separator, 'pointerdown', 100)
    await pointer(separator, 'pointermove', 140)
    await pointer(separator, 'pointerup', 140)
    expect(split.commits.at(-1)).toBe(184)
  })

  it.each([[300, 280], [160, Number.NaN], [200, 200]])('omits the separator when bounds %d..%d leave no usable range', async (min, max) => {
    await mountSplit({ min, max })
    expect(within(host).queryByRole('separator')).toBeNull()
    expect(host.querySelector('[data-resizer-track="sidebar-split-test"]')?.getAttribute('aria-hidden')).toBe('true')
  })

  it.each(['pointercancel', 'Escape'] as const)('%s rolls a held drag back without committing', async cancel => {
    const split = await mountSplit()
    const separator = split.separator()
    await pointer(separator, 'pointerdown', 100)
    await pointer(separator, 'pointermove', 140)
    expect(split.changes.at(-1)).toBe(240)
    if (cancel === 'Escape') await key(document.body, 'Escape')
    else await pointer(separator, 'pointercancel', 140)
    expect(split.changes.at(-1)).toBe(200)
    expect(separator.getAttribute('aria-valuenow')).toBe('200')
    expect(split.commits).toEqual([])
    await pointer(separator, 'pointermove', 180)
    expect(split.changes.at(-1)).toBe(200)
  })
})

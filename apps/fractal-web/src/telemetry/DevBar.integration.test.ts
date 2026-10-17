// @vitest-environment jsdom
/**
 * The recent-changes disclosure survives the move onto the shared developer bar: the real DevBar
 * composes the app's Changelog as a kit segment, the trigger carries the unseen indicator, opening
 * acknowledges and loads prose, Escape closes only the popover and returns focus to the trigger,
 * and an overflowed segment's unseen state is mirrored on the kit's +N control.
 */
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

const fixture = vi.hoisted(() => ({
  machineVersion: '0.1.0+test.123456789abc',
  entryId: '2026-10-14-shared-devbar',
  title: 'Shared developer bar',
}))

// Node tests stub only the CSS runtime; the composition and disclosure behaviour is under test.
vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))
vi.mock('virtual:build-identity', () => ({
  buildIdentity: { displayVersion: '0.1.0', machineVersion: fixture.machineVersion, dirty: false, rev: '123456789abc' },
  deploymentId: undefined,
}))
// jsdom has no canvas; the meter strip and its collector lease are covered by the kit's own suites.
vi.mock('@overeng/meters/react', () => ({
  MetersProvider: ({ children }: { readonly children: unknown }) => children,
  MeterStrip: () => null,
}))
// One deterministic entry instead of the app registry, in both the unseen ids and the lazy prose.
vi.mock('../changelog/metadata.ts', () => ({ changelogDeployment: 'main', changelogEntryIds: [fixture.entryId] }))
vi.mock('../changelog/build.ts', () => ({
  changelogDeployment: 'main',
  changelogManifest: {
    schemaVersion: 1,
    product: 'fractal-web',
    build: { machineVersion: fixture.machineVersion },
    entries: [{ id: fixture.entryId, date: '2026-10-14', visibility: 'public', title: fixture.title, summary: 'Diagnostics use the shared kit.' }],
  },
}))

let unmount: (() => Promise<void>) | undefined

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  // React Aria and the overflow focus fallback use CSS.escape; jsdom does not supply it.
  vi.stubGlobal('CSS', { escape: (value: string) => Array.from(value, (char) => /[a-zA-Z0-9_-]/.test(char) ? char : `\\${char.codePointAt(0)?.toString(16)} `).join('') })
  window.localStorage.clear()
})

afterEach(async () => {
  await unmount?.()
  unmount = undefined
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

/** Fresh modules per test: the bar's preference registry and seen cursor are module singletons. */
const mount = async () => {
  vi.resetModules()
  const [React, { createRoot }, { DevBar }] = await Promise.all([import('react'), import('react-dom/client'), import('./DevBar.tsx')])
  const host = document.createElement('div')
  document.body.append(host)
  const root = createRoot(host)
  await React.act(async () => root.render(React.createElement(DevBar, { defaultVisible: true })))
  unmount = async () => {
    await React.act(async () => root.unmount())
    host.remove()
  }
  return { act: React.act, host }
}

const changelogTrigger = (host: HTMLElement) => {
  const trigger = host.querySelector<HTMLButtonElement>('[data-devbar-segment="changelog"] [data-devbar-segment-content] button')
  if (trigger === null) throw new Error('changelog trigger did not mount in its devbar segment')
  return trigger
}
const recentChanges = () => document.querySelector<HTMLElement>('[role="dialog"][aria-label="Recent changes"]')

it('opens recent changes from the devbar segment, acknowledges them, and closes on Escape with focus restored', async () => {
  const { act, host } = await mount()
  const trigger = changelogTrigger(host)
  expect(trigger.getAttribute('aria-label')).toBe('Changes, unread')
  expect(trigger.querySelector('[data-testid="changelog-unseen"]')).not.toBeNull()

  // A kit panel stays open underneath; Escape must close only the changelog popover.
  const counters = [...host.querySelectorAll('button')].find((button) => button.textContent === 'Counters')
  await act(async () => counters?.click())
  expect(host.querySelector('section[aria-label="Counters"]')).not.toBeNull()

  trigger.focus()
  await act(async () => trigger.click())
  await vi.waitFor(async () => {
    await act(async () => {})
    expect(recentChanges()?.textContent).toContain(fixture.title)
  })
  expect(recentChanges()?.textContent).toContain("What's new")
  expect(trigger.getAttribute('aria-label')).toBe('Changes')
  expect(trigger.querySelector('[data-testid="changelog-unseen"]')).toBeNull()
  const stored = JSON.parse(window.localStorage.getItem('wf.ui@1') ?? '{}')
  expect(stored.entries?.['changelog.fractal-web.main']?.value).toEqual({ visited: true, entryIds: [fixture.entryId] })

  const dialog = recentChanges()!
  await act(async () => { dialog.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })) })
  await vi.waitFor(async () => {
    await act(async () => {})
    expect(recentChanges()).toBeNull()
    expect(document.activeElement).toBe(changelogTrigger(host))
  })
  expect(host.querySelector('section[aria-label="Counters"]')).not.toBeNull()
}, 30_000)

it('mirrors the unseen indicator on the overflow control when the changelog segment is hidden', async () => {
  // jsdom reports no row width, so any measured content overflows every segment.
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue(DOMRect.fromRect({ width: 48, height: 20 }))
  const { host } = await mount()
  const overflow = host.querySelector<HTMLButtonElement>('button[aria-haspopup="dialog"][aria-label*="more status items"]')
  expect(overflow?.getAttribute('aria-label')).toMatch(/: version, changelog, .*, including unseen changes$/)
  expect(overflow?.querySelector('[data-devbar-unseen]')).not.toBeNull()
}, 30_000)

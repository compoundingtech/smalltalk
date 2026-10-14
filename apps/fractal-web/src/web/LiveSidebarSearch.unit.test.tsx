// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { afterEach, describe, expect, it, vi } from 'vitest'

// Resolve StyleX variables the way the browser does: each defineVars group gets unique
// variable names with its defaults, and createTheme records the scheme's overrides.
const sx = vi.hoisted(() => ({ groups: 0, defaults: new Map<string, string>(), themes: new WeakMap<object, ReadonlyMap<string, string>>() }))
vi.mock('@stylexjs/stylex', () => {
  const flat = (values: readonly unknown[]): Record<string, unknown>[] => values.flatMap(value =>
    Array.isArray(value) ? flat(value) : typeof value === 'object' && value !== null && !sx.themes.has(value) ? [value as Record<string, unknown>] : [])
  return {
    create: (styles: unknown) => styles,
    keyframes: () => 'animation',
    defineVars: (vars: Record<string, string>) => {
      const group = sx.groups++
      return Object.fromEntries(Object.entries(vars).map(([key, value]) => {
        const name = `var(--g${group}-${key})`
        sx.defaults.set(name, value)
        return [key, name]
      }))
    },
    createTheme: (vars: Record<string, string>, values: Record<string, string>) => {
      const theme = {}
      sx.themes.set(theme, new Map(Object.entries(values).map(([key, value]) => [vars[key]!, value])))
      return theme
    },
    // ui-compat adapters merge atomic classes, so the resolved style travels in the class name.
    props: (...styles: unknown[]) => ({ className: JSON.stringify(Object.assign({}, ...flat(styles))) }),
  }
})

import { compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { colorVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { defaultFilters } from '../shell/sidebar/state.ts'
import { LiveSidebarToolbar } from './LiveAgentWorkspace.tsx'

const light = new Map(compositionLightTheme.flatMap(theme => [...sx.themes.get(theme)!]))
// The live shell's legacyBridge maps ui-compat adapter variables onto composition tokens.
const bridge: Record<string, string> = { 'var(--panel)': colorVars.raised, 'var(--line)': colorVars.borderStrong }
const resolve = (scheme: 'dark' | 'light', value: unknown): unknown => {
  const token = typeof value === 'string' ? bridge[value] ?? value : value
  return typeof token === 'string' && token.startsWith('var(') ? (scheme === 'light' ? light.get(token) : undefined) ?? sx.defaults.get(token) : token
}

let host: HTMLDivElement | undefined
afterEach(() => { host?.remove(); host = undefined; vi.unstubAllGlobals() })

describe('live sidebar search field', () => {
  it.each(['dark', 'light'] as const)('stands apart from the %s sidebar surface', async (scheme) => {
    vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
    vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
    host = document.createElement('div')
    document.body.append(host)
    const root = createRoot(host)
    await act(async () => root.render(<LiveSidebarToolbar filters={defaultFilters} update={() => {}} hosts={[]} />))
    const field = host.querySelector<HTMLInputElement>('input[aria-label="Search agents"]')!
    expect(field.getAttribute('aria-description')).toMatch(/machine:.*host:/)
    const style = JSON.parse(field.className) as Record<string, unknown>
    const sidebar = resolve(scheme, colorVars.sidebar)
    expect(sidebar).toMatch(/^#/)
    // Both resolve to real colors: an unresolved variable cannot pass as distinct.
    expect(resolve(scheme, style.backgroundColor)).toMatch(/^#/)
    expect(resolve(scheme, style.backgroundColor)).not.toBe(sidebar)
    expect(style.borderStyle).toBe('solid')
    expect(resolve(scheme, style.borderColor)).toMatch(/^(#|rgba\()/)
    expect(resolve(scheme, style.borderColor)).not.toBe(sidebar)
    await act(async () => root.unmount())
  })
})

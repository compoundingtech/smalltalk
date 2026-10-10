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
    props: (...styles: unknown[]) => ({ 'data-sx': JSON.stringify(Object.assign({}, ...flat(styles))) }),
  }
})

import { compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { colorVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { LiveSidebarSearch } from './LiveAgentWorkspace.tsx'

const light = new Map(compositionLightTheme.flatMap(theme => [...sx.themes.get(theme)!]))
const resolve = (scheme: 'dark' | 'light', value: unknown) =>
  typeof value === 'string' && value.startsWith('var(') ? (scheme === 'light' ? light.get(value) : undefined) ?? sx.defaults.get(value) : value

let host: HTMLDivElement | undefined
afterEach(() => { host?.remove(); host = undefined; vi.unstubAllGlobals() })

describe('live sidebar search', () => {
  it.each(['dark', 'light'] as const)('stands apart from the %s sidebar surface', async (scheme) => {
    vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
    host = document.createElement('div')
    document.body.append(host)
    const root = createRoot(host)
    await act(async () => root.render(<LiveSidebarSearch value="" onChange={() => {}} />))
    const field = host.querySelector('[data-sx]')!
    const style = JSON.parse(field.getAttribute('data-sx')!) as Record<string, unknown>
    const sidebar = resolve(scheme, colorVars.sidebar)
    expect(sidebar).toMatch(/^#/)
    expect(resolve(scheme, style.backgroundColor)).not.toBe(sidebar)
    expect(style.borderStyle).toBe('solid')
    expect(resolve(scheme, style.borderColor)).not.toBe(sidebar)
    await act(async () => root.unmount())
  })
})

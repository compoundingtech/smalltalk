// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'

// Resolve the real semantic defaults and light overrides, without faking CSS layout.
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
import { colorVars } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { AgentTodos } from './AgentTodos.tsx'
import { projectTodos } from './model.ts'
import { todoAgent, todoAgentRef } from './fixtures.ts'

const light = new Map(compositionLightTheme.flatMap(theme => [...sx.themes.get(theme)!]))
const resolve = (scheme: 'dark' | 'light', value: string): string => {
  const replacement = (scheme === 'light' ? light.get(value) : undefined) ?? sx.defaults.get(value)
  return replacement === undefined ? value : resolve(scheme, replacement)
}
const luminance = (hex: string): number => {
  expect(hex).toMatch(/^#[0-9a-f]{6}$/i)
  const channels = [1, 3, 5].map(index => {
    const value = parseInt(hex.slice(index, index + 2), 16) / 255
    return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4
  })
  return channels[0]! * 0.2126 + channels[1]! * 0.7152 + channels[2]! * 0.0722
}

it.each(['dark', 'light'] as const)('completed todo text meets WCAG AA on the %s Resources surface', async scheme => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const host = document.createElement('div')
  document.body.append(host)
  const root = createRoot(host)
  try {
    await act(async () => root.render(<AgentTodos placement="resources" state={projectTodos({ feed: { _tag: 'Observed', freshness: 'live', value: [todoAgent] }, agentRef: todoAgentRef })} />))
    const completed = host.querySelector('[aria-label="Completed"]')!.nextElementSibling!
    const style = JSON.parse(completed.getAttribute('data-sx')!) as { color: string }
    const foreground = luminance(resolve(scheme, style.color))
    const background = luminance(resolve(scheme, colorVars.raised))
    const contrast = (Math.max(foreground, background) + 0.05) / (Math.min(foreground, background) + 0.05)
    expect(contrast).toBeGreaterThanOrEqual(4.5)
  } finally {
    await act(async () => root.unmount())
    host.remove()
    vi.unstubAllGlobals()
  }
})

// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { afterEach, expect, it, vi } from 'vitest'
import { SidebarToolbar } from './Toolbar.tsx'
import { matchesRow, type SidebarRow } from './filter.ts'
import type { ShowRow } from './show.ts'
import { defaultFilters, type SidebarFilters } from './state.ts'
import { dailyDriver } from '../fixtures/layouts.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

afterEach(() => vi.unstubAllGlobals())

it('writes Show menu choices into the query, ANDs them with typed facets, counts them, and resets facets while preserving bare text and sort', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  // React Aria uses CSS.escape for generated collection keys; jsdom does not supply it.
  vi.stubGlobal('CSS', { escape: (value: string) => Array.from(value, char => /[a-zA-Z_-]/.test(char) ? char : `\\${char.codePointAt(0)?.toString(16)} `).join('') })
  const host = document.createElement('div')
  document.body.append(host)
  const root = createRoot(host)
  let filters: SidebarFilters = { ...defaultFilters, query: 'compiler machine:host-a HOST:host', sort: 'name' }
  const showRows: readonly ShowRow[] = [{ host: 'host-a', asking: false, others: true }, { host: 'host-b', asking: true, others: true }]
  const update = (value: SidebarFilters) => { filters = value; render() }
  const render = () => root.render(<SidebarToolbar filters={filters} update={update} hosts={['host-a', 'host-b']} showRows={showRows} />)
  const row: SidebarRow = { workspace: { id: 'agent/compiler', title: 'Compiler', host: 'host-a', layout: dailyDriver, unread: [], notifications: [] }, agent: undefined, status: 'idle', needsMe: false, asking: false }
  const item = (name: string) => document.body.querySelector<HTMLElement>(`[role="menuitemcheckbox"][aria-label="${name}"]`)
  try {
    await act(async () => render())
    expect(host.querySelector('button[aria-label="Filter agents · 2 active"]')).not.toBeNull()
    const trigger = [...host.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent?.startsWith('Show'))!
    expect(trigger.textContent).toContain(', 2 active')
    await act(async () => trigger.click())
    // Typed facets check their entries; an unobserved typed machine stays listed so it can be cleared.
    expect(item('host-a, 1')?.getAttribute('aria-checked')).toBe('true')
    expect(item('host, 2')?.getAttribute('aria-checked')).toBe('true')
    expect(item('host-b, 1')?.getAttribute('aria-checked')).toBe('false')
    expect(item('Waiting for answer, 0')?.getAttribute('aria-checked')).toBe('false')

    await act(async () => item('host-b, 1')!.click())
    expect(filters.query).toBe('machine:host-a,host,host-b compiler')
    expect(matchesRow({ row, filters, context: '' })).toBe(true)
    expect(item('Waiting for answer, 1')).not.toBeNull()

    await act(async () => item('Waiting for answer, 1')!.click())
    expect(filters.query).toBe('is:asking machine:host-a,host,host-b compiler')
    expect(matchesRow({ row, filters, context: '' })).toBe(false)
    expect(matchesRow({ row: { ...row, asking: true }, filters, context: '' })).toBe(true)
    expect(host.querySelector('button[aria-label="Filter agents · 2 active"]')).not.toBeNull()

    await act(async () => item('Waiting for answer, 1')!.click())
    expect(filters.query).toBe('machine:host-a,host,host-b compiler')

    const filterButton = host.querySelector<HTMLButtonElement>('button[aria-label="Filter agents · 1 active"]')!
    await act(async () => filterButton.click())
    const reset = [...document.body.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Reset filters')!
    await act(async () => reset.click())
    expect(filters).toEqual({ ...defaultFilters, query: 'compiler', sort: 'name' })
    expect(host.querySelector('button[aria-label="Filter agents"]')).not.toBeNull()
  } finally {
    await act(async () => root.unmount())
    host.remove()
  }
})

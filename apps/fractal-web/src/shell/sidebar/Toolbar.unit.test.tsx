// @vitest-environment jsdom
import * as React from 'react'
import { act } from 'react'
import { createRoot } from 'react-dom/client'
import { afterEach, expect, it, vi } from 'vitest'
import { SidebarToolbar } from './Toolbar.tsx'
import { matchesRow, type SidebarRow } from './filter.ts'
import { defaultFilters, type SidebarFilters } from './state.ts'
import { dailyDriver } from '../fixtures/layouts.ts'

vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

afterEach(() => vi.unstubAllGlobals())

it('ANDs the host menu with typed facets, counts them, and resets facets while preserving bare text and sort', async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
  const host = document.createElement('div')
  document.body.append(host)
  const root = createRoot(host)
  let filters: SidebarFilters = { ...defaultFilters, query: 'compiler machine:dev3 HOST:dev', sort: 'name' }
  const update = (value: SidebarFilters) => { filters = value; render() }
  const render = () => root.render(<SidebarToolbar filters={filters} update={update} hosts={['dev3', 'dev5']} />)
  const row: SidebarRow = { workspace: { id: 'agent/compiler', title: 'Compiler', host: 'dev3', layout: dailyDriver, unread: [], notifications: [] }, agent: undefined, status: 'idle', needsMe: false }
  try {
    await act(async () => render())
    const search = host.querySelector<HTMLInputElement>('input[aria-label="Search agents"]')!
    expect(search.getAttribute('aria-description')).toMatch(/names and IDs.*repo:.*machine:dev3.*host:dev3,dev5/)
    const filterButton = host.querySelector<HTMLButtonElement>('button[aria-label="Filter agents · 2 active"]')!
    expect(filterButton).not.toBeNull()
    await act(async () => filterButton.click())
    const menu = document.body.querySelector<HTMLSelectElement>('select[aria-label="Filter by host"]')!
    expect(menu).not.toBeNull()
    await act(async () => { menu.value = 'dev5'; menu.dispatchEvent(new Event('change', { bubbles: true })) })
    expect(filters.host).toBe('dev5')
    expect(matchesRow({ row, filters })).toBe(false)
    expect(host.querySelector('button[aria-label="Filter agents · 3 active"]')).not.toBeNull()
    await act(async () => { menu.value = 'dev3'; menu.dispatchEvent(new Event('change', { bubbles: true })) })
    expect(matchesRow({ row, filters })).toBe(true)
    const reset = [...document.body.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent === 'Reset filters')!
    await act(async () => reset.click())
    expect(filters).toEqual({ ...defaultFilters, query: 'compiler', sort: 'name' })
    expect(host.querySelector('button[aria-label="Filter agents"]')).not.toBeNull()
  } finally {
    await act(async () => root.unmount())
    host.remove()
  }
})

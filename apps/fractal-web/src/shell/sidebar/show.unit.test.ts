import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it } from 'vitest'

import { createPersistence } from '../../state/persistence.ts'
import { matchesShow, parseSidebarQuery, printSidebarQuery } from './filter.ts'
import { machineMemory, machinesToRemember, selectedShowKeys, showCounts, showMachines, withRememberedMachines, withShowKeys, type ShowRow } from './show.ts'

const toggle = (query: string, keys: readonly string[]) =>
  printSidebarQuery(withShowKeys({ parsedQuery: parseSidebarQuery(query), keys: new Set(keys) }))

describe('Show menu model', () => {
  it('reads checked entries from the query and writes them back as one OR-ed machine token', () => {
    expect([...selectedShowKeys(parseSidebarQuery('compiler IS:ASKING host:host-a,host-b'))]).toEqual(['is:asking', 'machine:host-a', 'machine:host-b'])
    expect(toggle('compiler', ['machine:host-a'])).toBe('machine:host-a compiler')
    expect(toggle('machine:host-a compiler', ['machine:host-a', 'is:asking', 'machine:host-b'])).toBe('is:asking machine:host-a,host-b compiler')
    expect(toggle('is:asking machine:host-a,host-b compiler', ['machine:host-b'])).toBe('machine:host-b compiler')
    expect(toggle('is:asking machine:host-a compiler', [])).toBe('compiler')
    // Choosing machines from an unconstrained menu ORs them.
    expect(toggle('', ['machine:host-a'])).toBe('machine:host-a')
    expect(toggle('machine:host-a', ['machine:host-a', 'machine:host-b'])).toBe('machine:host-a,host-b')
  })

  it('keeps typed machine AND groups when only Waiting for answer changes', () => {
    const rows = [{ host: 'host-a', asking: true }, { host: 'host-b', asking: true }] as const
    const visible = (query: string) => {
      const parsedQuery = parseSidebarQuery(query)
      return rows.filter((row) => matchesShow({ ...row, parsedQuery })).map((row) => row.host)
    }
    const andGroups = toggle('machine:host-a machine:host-b', ['is:asking', 'machine:host-a', 'machine:host-b'])
    expect(andGroups).toBe('is:asking machine:host-a machine:host-b')
    expect(visible(andGroups)).toEqual([])
    expect(toggle(andGroups, ['machine:host-a', 'machine:host-b'])).toBe('machine:host-a machine:host-b')
    // Overlapping prefix groups narrow to host-a and stay narrowed.
    const overlapping = toggle('machine:host machine:host-a', ['is:asking', 'machine:host', 'machine:host-a'])
    expect(overlapping).toBe('is:asking machine:host machine:host-a')
    expect(visible(overlapping)).toEqual(['host-a'])
    // A machine choice from the menu still rewrites the groups as one OR-ed token.
    expect(toggle('machine:host-a machine:host-b', ['machine:host-a'])).toBe('machine:host-a')
  })

  it('restores remembered machines over the machine tokens while keeping text and is:asking', () => {
    expect(withRememberedMachines({ query: '', remembered: [['host-a', 'host-b']] })).toBe('machine:host-a,host-b')
    expect(withRememberedMachines({ query: 'is:asking compiler machine:host-c', remembered: [['host-a']] })).toBe('is:asking machine:host-a compiler')
    expect(withRememberedMachines({ query: 'compiler machine:host-a,host-b', remembered: [['host-b', 'host-a']] })).toBe('compiler machine:host-a,host-b')
    expect(withRememberedMachines({ query: 'is:asking compiler', remembered: null })).toBe('is:asking compiler')
    expect(withRememberedMachines({ query: 'is:asking compiler machine:host-a', remembered: null })).toBe('is:asking compiler machine:host-a')
  })

  it('remembers and restores machine token groups without turning AND into OR', () => {
    const remembered = machinesToRemember('compiler machine:host-a machine:host-b', ['host-a', 'host-b'])
    expect(remembered).toEqual([['host-a'], ['host-b']])
    expect(withRememberedMachines({ query: 'is:asking', remembered })).toBe('is:asking machine:host-a machine:host-b')
    expect(withRememberedMachines({ query: 'machine:host-a,host-b', remembered })).toBe('machine:host-a machine:host-b')
  })

  it('remembers only complete known hosts, not partial prefixes or partially typed OR groups', () => {
    const hosts = ['host/HOST-A', 'host-b']
    expect(machinesToRemember('machine:h', hosts)).toBeNull()
    expect(machinesToRemember('machine:host-a,h', hosts)).toBeNull()
    expect(machinesToRemember('machine:unknown', hosts)).toBeNull()
    expect(machinesToRemember('machine:HOST-A,host-b', hosts)).toEqual([['host-a', 'host-b']])
    expect(machinesToRemember('compiler is:asking', hosts)).toEqual([])
  })

  it('does not confuse duplicate machine values with a different remembered choice', () => {
    const remembered = machinesToRemember('machine:host-a,host-a', ['host-a', 'host-b'])
    expect(withRememberedMachines({ query: 'machine:host-a,host-b', remembered })).toBe('machine:host-a,host-a')
    expect(withRememberedMachines({ query: 'machine:host-a', remembered })).toBe('machine:host-a')
  })

  it('clears a gateway query’s stale machine tokens when machines were explicitly cleared in this browser', () => {
    expect(withRememberedMachines({ query: 'machine:host-a', remembered: [] })).toBe('')
    expect(withRememberedMachines({ query: 'is:asking machine:host-a compiler', remembered: [] })).toBe('is:asking compiler')
    expect(withRememberedMachines({ query: 'is:asking compiler', remembered: [] })).toBe('is:asking compiler')
  })

  it('ANDs Waiting for answer with machines and ORs machines', () => {
    const parsedQuery = parseSidebarQuery('is:asking machine:host-a,host-b')
    expect(matchesShow({ host: 'host-a', asking: true, parsedQuery })).toBe(true)
    expect(matchesShow({ host: 'host/HOST-B', asking: true, parsedQuery })).toBe(true)
    expect(matchesShow({ host: 'host-a', asking: false, parsedQuery })).toBe(false)
    expect(matchesShow({ host: 'host-c', asking: true, parsedQuery })).toBe(false)
  })

  it('lists observed machines once, sorted, and keeps selected unobserved machines clearable', () => {
    expect(showMachines({ hosts: ['host-b', 'host/host-a', 'HOST-A'], parsedQuery: parseSidebarQuery('machine:host-z') })).toEqual([
      { value: 'host-a', label: 'HOST-A' },
      { value: 'host-b', label: 'host-b' },
      { value: 'host-z', label: 'host-z' },
    ])
  })

  it('counts each entry against every other active filter', () => {
    const rows: readonly ShowRow[] = [
      { host: 'host-a', asking: true, others: true },
      { host: 'host-a', asking: false, others: true },
      { host: 'host-b', asking: true, others: true },
      { host: 'host-b', asking: true, others: false },
    ]
    const counts = (query: string) => {
      const result = showCounts({ rows, parsedQuery: parseSidebarQuery(query), machines: ['host-a', 'host-b', 'host-c'] })
      return { asking: result.asking, machines: Object.fromEntries(result.machines) }
    }
    expect(counts('')).toEqual({ asking: 2, machines: { 'host-a': 2, 'host-b': 1, 'host-c': 0 } })
    expect(counts('is:asking')).toEqual({ asking: 2, machines: { 'host-a': 1, 'host-b': 1, 'host-c': 0 } })
    expect(counts('machine:host-a')).toEqual({ asking: 1, machines: { 'host-a': 2, 'host-b': 1, 'host-c': 0 } })
  })
})

describe('remembered machines', () => {
  const restore = (raw: string | null) => {
    const store = createPersistence({ adapter: { read: () => raw, write: () => {}, subscribe: () => () => {} } })
    const machines = AtomRegistry.make().get(store.atom(machineMemory))
    const result = { machines, query: withRememberedMachines({ query: '', remembered: machines }), error: store.error }
    store.dispose()
    return result
  }
  const entry = (version: number, value: unknown) =>
    JSON.stringify({ version: 1, entries: { [machineMemory.key]: { version, value, clock: 1, writer: 'other-tab' } } })

  it('restores a valid list as a machine token', () => {
    expect(restore(entry(2, [['host-a', 'host-b']]))).toEqual({ machines: [['host-a', 'host-b']], query: 'machine:host-a,host-b', error: undefined })
    expect(restore(entry(2, []))).toEqual({ machines: [], query: '', error: undefined })
    expect(restore(null)).toEqual({ machines: null, query: '', error: undefined })
  })

  it.each([
    ['a value that is not a machine list', entry(2, [['host-a', 'host b', 7]])],
    ['an uppercase value', entry(2, [['HOST-A']])],
    ['an empty machine token', entry(2, [[]])],
    ['a newer entry version', entry(3, [['host-a']])],
    ['a document that is not JSON', '{"version":1,'],
  ])('restores nothing from %s and reports the error', (_, raw) => {
    const restored = restore(raw)
    expect(restored.machines).toBeNull()
    expect(restored.query).toBe('')
    expect(restored.error).toBeDefined()
  })
})

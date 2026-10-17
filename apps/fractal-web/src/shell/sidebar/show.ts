import { useAtom } from '@effect/atom-react'
import { Schema } from 'effect'

import { persistedAtom } from '../../state/persistence.ts'
import {
  askingToken,
  matchesSidebarHost,
  parseSidebarQuery,
  printSidebarQuery,
  sidebarHostValue,
  type SidebarQuery,
} from './filter.ts'

/** Show menu keys are the search tokens themselves, so menu and search share one grammar. */
export const machineKey = (machine: string): string => `machine:${machine}`
/** Every machine value in the query's machine tokens, first occurrence first. */
export const selectedMachines = (parsedQuery: SidebarQuery): readonly string[] => [...new Set(parsedQuery.machines.flat())]
const sameMachines = (left: readonly string[], right: readonly string[]): boolean =>
  left.every((machine) => right.includes(machine)) && right.every((machine) => left.includes(machine))
/** Checked Show entries derived from the query. */
export const selectedShowKeys = (parsedQuery: SidebarQuery): ReadonlySet<string> =>
  new Set([...(parsedQuery.asking ? [askingToken] : []), ...selectedMachines(parsedQuery).map(machineKey)])
/** Menu selection back to the query: text stays; typed machine groups stay unless the machine choice changed,
 * in which case the checked machines become one OR-ed machine token. */
export const withShowKeys = ({
  parsedQuery,
  keys,
}: {
  readonly parsedQuery: SidebarQuery
  readonly keys: ReadonlySet<string>
}): SidebarQuery => {
  const machines = [...keys].flatMap((key) => (key.startsWith('machine:') ? [key.slice('machine:'.length)] : []))
  const unchanged = sameMachines(machines, selectedMachines(parsedQuery))
  return {
    query: parsedQuery.query,
    asking: keys.has(askingToken),
    machines: unchanged ? parsedQuery.machines : machines.length === 0 ? [] : [machines],
  }
}

/** Machine entries: every observed host, plus selected machines no host reports, so they stay clearable. */
export const showMachines = ({
  hosts,
  parsedQuery,
}: {
  readonly hosts: readonly string[]
  readonly parsedQuery: SidebarQuery
}): readonly { readonly value: string; readonly label: string }[] => {
  const entries = new Map<string, string>()
  for (const host of hosts.toSorted()) if (!entries.has(sidebarHostValue(host))) entries.set(sidebarHostValue(host), host)
  for (const machine of selectedMachines(parsedQuery)) if (!entries.has(machine)) entries.set(machine, machine)
  return [...entries].map(([value, label]) => ({ value, label }))
}

/** One roster row as the Show menu counts it; `others` = passes every filter outside the Show menu. */
export interface ShowRow {
  readonly host: string
  readonly asking: boolean
  readonly others: boolean
}
/** Faceted counts: each entry counts rows that pass the other filters and that entry. */
export const showCounts = ({
  rows,
  parsedQuery,
  machines,
}: {
  readonly rows: readonly ShowRow[]
  readonly parsedQuery: SidebarQuery
  readonly machines: readonly string[]
}): { readonly asking: number; readonly machines: ReadonlyMap<string, number> } => {
  let asking = 0
  const counts = new Map(machines.map((machine) => [machine, 0]))
  for (const row of rows) {
    if (!row.others) continue
    if (row.asking && matchesSidebarHost(row.host, parsedQuery.machines)) asking++
    if (parsedQuery.asking && !row.asking) continue
    for (const machine of machines) if (matchesSidebarHost(row.host, [[machine]])) counts.set(machine, counts.get(machine)! + 1)
  }
  return { asking, machines: counts }
}

/** Browser-local machine groups: `null` = no readable choice; `[]` = explicitly cleared.
 * Version 2 preserves AND between tokens and OR within each token. */
export const machineMemory = {
  key: 'sidebar.show.machines@2',
  version: 2,
  schema: Schema.NullOr(Schema.Array(Schema.Array(Schema.String.check(Schema.isPattern(/^[^\s,A-Z]+$/u))).check(Schema.isMinLength(1)))),
  defaultValue: null,
}
const rememberedMachines = persistedAtom(machineMemory)
const sameMachineGroups = (left: SidebarQuery['machines'], right: SidebarQuery['machines']): boolean =>
  left.length === right.length && left.every((group, index) => sameMachines(group, right[index]!))
/** Replace only machine tokens, preserving the remembered AND/OR structure. */
export const withRememberedMachines = ({
  query,
  remembered,
}: {
  readonly query: string
  readonly remembered: SidebarQuery['machines'] | null
}): string => {
  const parsedQuery = parseSidebarQuery(query)
  if (remembered === null || sameMachineGroups(remembered, parsedQuery.machines)) return query
  return printSidebarQuery({ ...parsedQuery, machines: remembered })
}
/** Partial or unknown typed hosts still filter live, but never overwrite the last complete choice. */
export const machinesToRemember = (query: string, hosts: readonly string[]): SidebarQuery['machines'] | null => {
  const machines = parseSidebarQuery(query).machines
  const known = new Set(hosts.map(sidebarHostValue))
  return machines.every((group) => group.every((machine) => known.has(machine))) ? machines : null
}
/** The query stays the only live machine state; memory mirrors complete machine tokens. */
export const useMachineMemory = () => {
  const [remembered, setRemembered] = useAtom(rememberedMachines)
  const remember = (query: string, hosts: readonly string[]) => {
    const next = machinesToRemember(query, hosts)
    if (next !== null && (remembered === null || !sameMachineGroups(next, remembered))) setRemembered(next)
  }
  return { remembered, remember }
}

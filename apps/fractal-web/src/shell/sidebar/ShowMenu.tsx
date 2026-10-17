import { ShowMenu, type ShowMenuProps } from '@smalltalk/fractal-ui/assistant-ui/shell'

import { askingToken, parseSidebarQuery, printSidebarQuery } from './filter.ts'
import { machineKey, selectedShowKeys, showCounts, showMachines, withShowKeys, type ShowRow } from './show.ts'

/** The sidebar's Show menu: state is read from and written to the search query, never held separately. */
export const SidebarShowMenu = ({
  query,
  onQueryChange,
  rows,
  hosts,
  popoverTheme,
}: {
  readonly query: string
  readonly onQueryChange: (query: string) => void
  readonly rows: readonly ShowRow[]
  readonly hosts: readonly string[]
  readonly popoverTheme?: ShowMenuProps['popoverTheme']
}) => {
  const parsedQuery = parseSidebarQuery(query)
  const machines = showMachines({ hosts, parsedQuery })
  const counts = showCounts({ rows, parsedQuery, machines: machines.map((machine) => machine.value) })
  return (
    <ShowMenu
      groups={[
        { id: 'agents', title: 'Agents', entries: [{ id: askingToken, label: 'Waiting for answer', count: counts.asking }] },
        ...(machines.length === 0
          ? []
          : [{
              id: 'machines',
              title: 'Machine',
              entries: machines.map((machine) => ({
                id: machineKey(machine.value),
                label: machine.label,
                count: counts.machines.get(machine.value) ?? 0,
              })),
            }]),
      ]}
      selectedKeys={selectedShowKeys(parsedQuery)}
      onSelectionChange={(keys) => onQueryChange(printSidebarQuery(withShowKeys({ parsedQuery, keys })))}
      {...(popoverTheme === undefined ? {} : { popoverTheme })}
    />
  )
}

import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { DialogTrigger, Dialog, Popover } from 'react-aria-components'
import { compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'

import {
  Button,
  Checkbox,
  Input,
  Menu,
  MenuItem,
  MenuPopover,
  MenuTrigger,
} from '../../ui-compat/components.tsx'
import { scale, tokens } from '../../ui-compat/tokens.stylex.ts'

import { IconButton } from '../../workbench-kit/IconButton.tsx'
import { parseSidebarQuery, sortModes, type SortMode } from './filter.ts'
import { SidebarShowMenu } from './ShowMenu.tsx'
import type { ShowRow } from './show.ts'
import { defaultFilters, type SidebarFilters } from './state.ts'
import { statuses, type AgentStatus } from './StatusIcon.tsx'

const readLightScheme = () => document.documentElement.dataset.scheme !== 'dark'
const subscribeScheme = (notify: () => void) => {
  const observer = new MutationObserver(notify)
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-scheme'] })
  return () => observer.disconnect()
}

/** Sidebar search, Show, visibility, status and view-only ordering controls. */
export const SidebarToolbar = ({
  filters,
  update,
  hosts,
  showRows,
}: {
  readonly filters: SidebarFilters
  readonly update: (value: SidebarFilters) => void
  readonly hosts: readonly string[]
  readonly showRows: readonly ShowRow[]
}) => {
  const light = React.useSyncExternalStore(subscribeScheme, readLightScheme, () => true)
  const search = React.useRef<HTMLDivElement>(null)
  const shortcut = React.useCallback((node: HTMLDivElement | null) => {
    if (node === null) return
    const listener = (event: KeyboardEvent) => {
      const target = event.target
      if (
        event.key !== '/' ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        (target instanceof HTMLElement &&
          (target.isContentEditable || target.closest('input,textarea,select,[role="textbox"]')))
      )
        return
      event.preventDefault()
      search.current?.querySelector('input')?.focus()
    }
    document.addEventListener('keydown', listener)
    return () => document.removeEventListener('keydown', listener)
  }, [])
  const parsedQuery = parseSidebarQuery(filters.query)
  const activeCount =
    Number(filters.hideEnded) +
    Number(filters.hideRetired) +
    Number(parsedQuery.asking) +
    parsedQuery.machines.length +
    filters.statuses.length
  return (
    <div ref={shortcut} {...stylex.props(styles.toolbar)}>
      <div {...stylex.props(styles.controls)}>
        <SidebarShowMenu
          query={filters.query}
          onQueryChange={(query) => update({ ...filters, query })}
          rows={showRows}
          hosts={hosts}
          {...(light ? { popoverTheme: compositionLightTheme } : {})}
        />
        <div {...stylex.props(styles.menus)}>
          <DialogTrigger>
            <IconButton
              size="md"
              label={`Filter agents${activeCount > 0 ? ` · ${activeCount} active` : ''}`}
              isActive={activeCount > 0}
              icon={<FilterIcon />}
            />
            <Popover placement="bottom end" {...stylex.props(styles.popover)}>
              <Dialog aria-label="Agent filters" {...stylex.props(styles.dialog)}>
                <Checkbox
                  isSelected={filters.hideEnded}
                  onChange={(hideEnded) => update({ ...filters, hideEnded })}
                >
                  Hide ended
                </Checkbox>
                <Checkbox
                  isSelected={filters.hideRetired}
                  onChange={(hideRetired) => update({ ...filters, hideRetired })}
                >
                  Hide retired
                </Checkbox>
                <fieldset {...stylex.props(styles.statuses)}>
                  <legend>Status</legend>
                  {(Object.keys(statuses) as AgentStatus[]).map((status) => (
                    <Checkbox
                      key={status}
                      isSelected={filters.statuses.includes(status)}
                      onChange={(selected) =>
                        update({
                          ...filters,
                          statuses: selected
                            ? [...filters.statuses, status]
                            : filters.statuses.filter((value) => value !== status),
                        })
                      }
                    >
                      {statuses[status].label}
                    </Checkbox>
                  ))}
                </fieldset>
                <Button
                  size="sm"
                  variant="secondary"
                  onPress={() =>
                    update({ ...defaultFilters, query: parseSidebarQuery(filters.query).query, sort: filters.sort })
                  }
                >
                  Reset filters
                </Button>
              </Dialog>
            </Popover>
          </DialogTrigger>
          <MenuTrigger>
            <IconButton
              size="md"
              label={`Sort agents · ${sortModes[filters.sort]}`}
              isActive={filters.sort !== 'manual'}
              icon={<SortIcon />}
            />
            <MenuPopover>
              <Menu
                aria-label="Sort agents within each folder"
                selectionMode="single"
                selectedKeys={[filters.sort]}
                onAction={(key) => update({ ...filters, sort: key as SortMode })}
              >
                {Object.entries(sortModes).map(([value, label]) => (
                  <MenuItem
                    key={value}
                    id={value}
                    textValue={label}
                    suffix={filters.sort === value ? <span aria-hidden="true">✓</span> : undefined}
                  >
                    {label}
                  </MenuItem>
                ))}
              </Menu>
            </MenuPopover>
          </MenuTrigger>
        </div>
      </div>
      <div ref={search}>
        <Input
          aria-label="Search agents and folders"
          aria-description="Use is:asking for agents waiting for an answer, and machine:synthetic-138/1 or host:synthetic-138/1,synthetic-138/2 to filter by host; combine with search text."
          placeholder="Search agents…  /"
          size="sm"
          value={filters.query}
          onChange={(query) => update({ ...filters, query })}
          onKeyDown={(event) => {
            if (event.key === 'Escape') update({ ...filters, query: '' })
          }}
        />
      </div>
    </div>
  )
}
const FilterIcon = () => (
  <svg
    aria-hidden="true"
    width="16"
    height="16"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.5"
    strokeLinecap="round"
  >
    <path d="M2 4h12M4 8h8M6 12h4" />
  </svg>
)
const SortIcon = () => (
  <svg
    aria-hidden="true"
    width="16"
    height="16"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.5"
    strokeLinecap="round"
    strokeLinejoin="round"
  >
    <path d="M4 3v10l-2-2m2 2 2-2M8 4h6M8 8h4M8 12h2" />
  </svg>
)

const styles = stylex.create({
  toolbar: {
    display: 'flex',
    flexDirection: 'column',
    gap: scale.space2,
    paddingInline: scale.space2,
    paddingBottom: scale.space2,
  },
  controls: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'space-between',
    gap: scale.space2,
  },
  popover: {
    backgroundColor: tokens['--ds-background-100'],
    borderRadius: scale.radiusDefault,
    borderWidth: 1,
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    padding: scale.space3,
    boxShadow: '0 8px 24px rgb(0 0 0 / .15)',
    width: '240px',
    maxHeight: '80vh',
    overflowY: 'auto',
  },
  dialog: { display: 'flex', flexDirection: 'column', gap: scale.space3, outline: 'none' },
  statuses: {
    display: 'flex',
    flexDirection: 'column',
    gap: scale.space2,
    border: 0,
    padding: 0,
    margin: 0,
  },
  menus: { display: 'flex', alignItems: 'center', gap: scale.space1 },
})

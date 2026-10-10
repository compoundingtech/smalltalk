import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { DialogTrigger, Dialog, Popover, ToggleButton } from 'react-aria-components'

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
import { sortModes, type SortMode } from './filter.ts'
import { defaultFilters, type SidebarFilters } from './state.ts'
import { statuses, type AgentStatus } from './StatusIcon.tsx'

/** Sidebar search, visibility, host, status and view-only ordering controls. */
export const SidebarToolbar = ({
  filters,
  update,
  hosts,
}: {
  readonly filters: SidebarFilters
  readonly update: (value: SidebarFilters) => void
  readonly hosts: readonly string[]
}) => {
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
  const activeCount =
    Number(filters.hideEnded) +
    Number(filters.hideRetired) +
    Number(filters.host !== '') +
    filters.statuses.length
  return (
    <div ref={shortcut} {...stylex.props(styles.toolbar)}>
      <div {...stylex.props(styles.controls)}>
        <ToggleButton
          aria-label="Needs me only"
          isSelected={filters.needsMe}
          onChange={(needsMe) => update({ ...filters, needsMe })}
          {...stylex.props(styles.toggle)}
        >
          Needs me
        </ToggleButton>
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
                <label {...stylex.props(styles.label)}>
                  Host
                  <select
                    aria-label="Filter by host"
                    value={filters.host}
                    onChange={(event) => update({ ...filters, host: event.target.value })}
                    {...stylex.props(styles.select)}
                  >
                    <option value="">All hosts</option>
                    {hosts.map((host) => (
                      <option key={host}>{host}</option>
                    ))}
                  </select>
                </label>
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
                    update({ ...defaultFilters, query: filters.query, sort: filters.sort })
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
  toggle: {
    border: 0,
    borderRadius: scale.radiusSm,
    padding: '5px 8px',
    backgroundColor: {
      default: 'transparent',
      ':hover': tokens['--ds-gray-alpha-100'],
      ':is([data-selected])': tokens['--ds-gray-alpha-200'],
    },
    color: tokens['--ds-gray-1000'],
    fontSize: '0.75rem',
    cursor: 'pointer',
    outlineColor: tokens['--ds-focus-color'],
    whiteSpace: 'nowrap',
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
  label: {
    display: 'flex',
    flexDirection: 'column',
    gap: scale.space1,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-900'],
  },
  select: {
    minWidth: 0,
    width: '100%',
    fontSize: '0.75rem',
    padding: '4px',
    borderWidth: 1,
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    borderRadius: scale.radiusSm,
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-background-100'],
    outlineColor: tokens['--ds-focus-color'],
  },
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

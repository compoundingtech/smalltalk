// Editor tabs on React Aria `Tabs`: RAC owns the tablist semantics, roving focus, arrow, Home and End
// navigation and automatic activation; react-aria `useDrag`/`useDrop` own drag and drop (mouse,
// touch, and the keyboard/screen-reader drag mode started with Enter on a tab, dropped with Enter
// on a highlighted target). wf adds Alt+Shift+←/→ (↑/↓ when vertical) reorder announced through a
// live region, Delete to close, dirty/live/attention markers and an "Open surfaces"
// overflow menu. The close glyph is pointer-only (`aria-hidden`) so a
// tab holds no nested interactive element.

import * as stylex from '@stylexjs/stylex'
import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
} from 'react'
import { mergeProps, useDrag, useDrop, type DropItem } from 'react-aria'
import { Tab, TabList, TabPanel, Tabs, type Key } from 'react-aria-components'

import { Menu, MenuItem, MenuPopover, MenuTrigger } from '../ui-compat/components.tsx'
import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import { HarnessIcon } from './brand-icons.tsx'
import { IconButton } from './IconButton.tsx'
import { CloseIcon, MoreIcon } from './icons.tsx'

/** One editor surface represented in a tab strip. */
export interface EditorTabItem {
  readonly id: string
  readonly title: string
  /** Disambiguating suffix (host, editor kind), drawn dimmed. */
  readonly detail?: string
  /** Gateway harness id, drawn as its brand mark after `detail`. */
  readonly harness?: string
  readonly icon?: ReactNode
  /** Unsaved local state (e.g. a composer draft); replaces the close glyph with a dot. */
  readonly dirty?: boolean
  /** `live`: following a working subject; `unavailable`: stale or absent (dimmed). */
  readonly status?: 'live' | 'unavailable'
  /** Needs the user (notification ring). */
  readonly attention?: boolean
}

/** Source tab identity transferred between strips during drag and drop. */
export interface TabDragPayload {
  readonly groupId: string
  readonly tabId: string
}

/** Strip presentation: VS Code tabs, compact pills, or a vertical session list. */
export type EditorTabsVariant = 'classic' | 'compact' | 'vertical'

/** Controlled tab strip state, drag configuration, actions, and active panel. */
export interface EditorTabsProps {
  readonly label: string
  readonly groupId: string
  readonly tabs: readonly EditorTabItem[]
  readonly activeId: string | null
  /** `classic` VS Code boxed, `compact` Zed pills, `vertical` compact session list above the pane. */
  readonly variant: EditorTabsVariant
  readonly isGroupFocused: boolean
  /** MIME type shared by every strip that accepts drops from this one. */
  readonly dragType: string
  readonly onActivate: (id: string) => void
  readonly onClose: (id: string) => void
  /** Same-group reorder or a drop from another group; `toIndex` is the final index. */
  readonly onMove: (move: { readonly source: TabDragPayload; readonly toIndex: number }) => void
  readonly actions?: ReactNode
  /** The active editor, rendered as the RAC `TabPanel`. */
  readonly children: ReactNode
}

const styles = stylex.create({
  tabs: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0 },
  strip: {
    position: 'relative',
    display: 'flex',
    alignItems: 'stretch',
    flexShrink: 0,
    minWidth: 0,
  },
  stripClassic: {
    height: '1.75rem',
    backgroundColor: tokens['--ds-background-200'],
    borderBottomWidth: '1px',
    borderBottomStyle: 'solid',
    borderBottomColor: tokens['--ds-gray-alpha-400'],
  },
  stripCompact: {
    height: '1.75rem',
    paddingInline: scale.space1,
    gap: '2px',
    alignItems: 'center',
    backgroundColor: tokens['--ds-background-200'],
  },
  stripVertical: {
    display: 'grid',
    gridTemplateColumns: 'minmax(0, 1fr) auto',
    gridTemplateRows: '28px auto',
    backgroundColor: tokens['--ds-background-200'],
    borderBottomWidth: '1px',
    borderBottomStyle: 'solid',
    borderBottomColor: tokens['--ds-gray-alpha-400'],
  },
  list: {
    position: 'relative',
    display: 'flex',
    alignItems: 'stretch',
    flexGrow: 1,
    minWidth: 0,
    overflowX: 'auto',
    overflowY: 'hidden',
    scrollbarWidth: 'none',
    outlineStyle: 'none',
  },
  listCompact: { alignItems: 'center', gap: '2px' },
  listVertical: {
    flexDirection: 'column',
    overflowX: 'hidden',
    overflowY: 'auto',
    gap: 0,
    paddingInline: scale.space1,
    gridColumn: '1 / -1',
    gridRow: 2,
    maxHeight: '140px',
  },
  listDropTarget: { backgroundColor: tokens['--ds-gray-alpha-100'] },
  tab: {
    position: 'relative',
    display: 'flex',
    alignItems: 'center',
    gap: '6px',
    flexShrink: 0,
    minWidth: 0,
    maxWidth: '15rem',
    paddingInlineStart: scale.space2,
    paddingInlineEnd: '6px',
    fontSize: '0.8125rem',
    lineHeight: '1.25rem',
    whiteSpace: 'nowrap',
    cursor: 'default',
    userSelect: 'none',
    color: tokens['--ds-gray-900'],
    outlineStyle: 'none',
  },
  focusVisible: { boxShadow: `inset 0 0 0 2px ${tokens['--ds-focus-color']}` },
  tabClassic: {
    borderRightWidth: '1px',
    borderRightStyle: 'solid',
    borderRightColor: tokens['--ds-gray-alpha-400'],
    backgroundColor: { default: 'transparent', ':hover': tokens['--ds-gray-alpha-100'] },
  },
  tabClassicActive: {
    color: tokens['--ds-gray-1000'],
    backgroundColor: {
      default: tokens['--ds-background-100'],
      ':hover': tokens['--ds-background-100'],
    },
    // Covers the strip's bottom border so the active tab merges into the editor body.
    marginBottom: '-1px',
  },
  tabClassicFocused: { boxShadow: `inset 0 1px 0 ${tokens['--ds-blue-700']}` },
  tabCompact: {
    height: '1.75rem',
    boxSizing: 'border-box',
    borderBlockWidth: '2px',
    borderInlineWidth: 0,
    borderStyle: 'solid',
    borderColor: 'transparent',
    backgroundClip: 'padding-box',
    borderRadius: scale.radiusDefault,
    paddingInlineStart: scale.space2,
    backgroundColor: { default: 'transparent', ':hover': tokens['--ds-gray-alpha-100'] },
  },
  tabCompactActive: {
    color: tokens['--ds-gray-1000'],
    backgroundColor: {
      default: tokens['--ds-gray-alpha-200'],
      ':hover': tokens['--ds-gray-alpha-200'],
    },
  },
  tabVertical: {
    maxWidth: 'none',
    alignItems: 'center',
    minHeight: '1.75rem',
    paddingBlock: 0,
    paddingInlineStart: scale.space2,
    borderRadius: scale.radiusDefault,
    whiteSpace: 'normal',
    backgroundColor: { default: 'transparent', ':hover': tokens['--ds-gray-alpha-100'] },
  },
  tabVerticalActive: {
    color: tokens['--ds-gray-1000'],
    backgroundColor: {
      default: tokens['--ds-gray-alpha-200'],
      ':hover': tokens['--ds-gray-alpha-200'],
    },
  },
  dragging: { opacity: 0.5 },
  verticalText: {
    display: 'flex',
    flexGrow: 1,
    minWidth: 0,
    alignItems: 'baseline',
    gap: scale.space1,
  },
  verticalHeading: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'space-between',
    height: '1.75rem',
    paddingInline: scale.space2,
    fontSize: '0.8125rem',
    fontWeight: 500,
    color: tokens['--ds-gray-900'],
  },
  liveRing: { boxShadow: `0 0 0 2px ${tokens['--ds-green-300']}` },
  title: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  // Colour, not opacity: a 55% tab title fails WCAG contrast (axe color-contrast).
  unavailable: { color: tokens['--ds-gray-800'], fontStyle: 'italic' },
  detail: {
    color: tokens['--ds-gray-800'],
    fontSize: '0.75rem',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
  },
  icon: { display: 'inline-flex', flexShrink: 0, color: tokens['--ds-gray-900'], marginTop: '2px' },
  live: {
    width: '6px',
    height: '6px',
    flexShrink: 0,
    borderRadius: scale.radiusFull,
    backgroundColor: tokens['--ds-green-700'],
    animationName: stylex.keyframes({
      '0%': { opacity: 1 },
      '50%': { opacity: 0.35 },
      '100%': { opacity: 1 },
    }),
    animationDuration: { default: '1.6s', '@media (prefers-reduced-motion: reduce)': '0s' },
    animationIterationCount: 'infinite',
  },
  // Attention uses amber so a notification cannot be mistaken for the blue focused-pane ring.
  attention: { boxShadow: `inset 0 0 0 1px ${tokens['--ds-amber-700']}` },
  attentionDot: {
    width: '6px',
    height: '6px',
    flexShrink: 0,
    borderRadius: scale.radiusFull,
    backgroundColor: tokens['--ds-amber-700'],
    boxShadow: `0 0 0 2px ${tokens['--ds-amber-300']}`,
  },
  trailing: {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    flexShrink: 0,
    width: '1.125rem',
    height: '1.125rem',
    borderRadius: scale.radiusSm,
    color: tokens['--ds-gray-900'],
    backgroundColor: { default: 'transparent', ':hover': tokens['--ds-gray-alpha-300'] },
  },
  closeHidden: { opacity: 0 },
  closeShown: { opacity: 1 },
  dirtyDot: {
    width: '8px',
    height: '8px',
    borderRadius: scale.radiusFull,
    backgroundColor: tokens['--ds-gray-1000'],
  },
  dropIndicatorX: (left: number) => ({
    position: 'absolute',
    top: '4px',
    bottom: '4px',
    left,
    width: '2px',
    borderRadius: '1px',
    backgroundColor: tokens['--ds-blue-700'],
    pointerEvents: 'none',
  }),
  dropIndicatorY: (top: number) => ({
    position: 'absolute',
    left: '4px',
    right: '4px',
    top,
    height: '2px',
    borderRadius: '1px',
    backgroundColor: tokens['--ds-blue-700'],
    pointerEvents: 'none',
  }),
  actions: {
    display: 'flex',
    alignItems: 'center',
    gap: '2px',
    flexShrink: 0,
    paddingInline: scale.space1,
  },
  actionsVertical: { gridColumn: 2, gridRow: 1, justifyContent: 'flex-end' },
  panel: {
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minWidth: 0,
    minHeight: 0,
    outlineStyle: 'none',
  },
  srOnly: {
    position: 'absolute',
    width: '1px',
    height: '1px',
    overflow: 'hidden',
    clipPath: 'inset(50%)',
    whiteSpace: 'nowrap',
  },
})

/** Decodes a tab drag payload, rejecting malformed or non-tab drag data. */
export const parseTabDragPayload = (raw: string): TabDragPayload | null => {
  try {
    const value: unknown = JSON.parse(raw)
    return typeof value === 'object' &&
      value !== null &&
      'groupId' in value &&
      'tabId' in value &&
      typeof value.groupId === 'string' &&
      typeof value.tabId === 'string'
      ? { groupId: value.groupId, tabId: value.tabId }
      : null
  } catch {
    return null
  }
}

/** Reads the first drop item carrying `dragType` (react-aria `DropItem`s are async). */
export const readTabDrop = async ({
  items,
  dragType,
}: {
  readonly items: ReadonlyArray<DropItem>
  readonly dragType: string
}): Promise<TabDragPayload | null> => {
  for (const item of items) {
    if (item.kind === 'text' && item.types.has(dragType))
      return parseTabDragPayload(await item.getText(dragType))
  }
  return null
}

/** react-aria DOM props carry explicit `undefined`s that RAC's exact optional props reject. */
const definedProps = (props: object): object =>
  Object.fromEntries(Object.entries(props).filter(([, value]) => value !== undefined))

/** Accessible editor tab strip with drag, keyboard reorder, and overflow navigation. */
export const EditorTabs = ({
  label,
  groupId,
  tabs,
  activeId,
  variant,
  isGroupFocused,
  dragType,
  onActivate,
  onClose,
  onMove,
  actions,
  children,
}: EditorTabsProps) => {
  const list = useRef<HTMLDivElement>(null)
  const [drop, setDrop] = useState<{ readonly index: number; readonly offset: number } | null>(null)
  const [announcement, setAnnouncement] = useState('')
  const vertical = variant === 'vertical'

  // Keep the active tab visible when it changes (keyboard cycling, quick-open, overflow menu).
  useEffect(() => {
    if (activeId === null) return
    list.current
      ?.querySelector(`[data-tab-id="${CSS.escape(activeId)}"]`)
      ?.scrollIntoView({ block: 'nearest', inline: 'nearest' })
  }, [activeId])

  /** Insertion index and indicator offset for a point relative to the strip (react-aria drop coordinates). */
  const dropTarget = ({ x, y }: { readonly x: number; readonly y: number }) => {
    const container = list.current
    if (container === null) return { index: tabs.length, offset: 0 }
    const box = container.getBoundingClientRect()
    const nodes = [...container.querySelectorAll<HTMLElement>('[data-tab-id]')]
    const pointer = vertical ? box.top + y : box.left + x
    const scroll = vertical ? container.scrollTop : container.scrollLeft
    const start = vertical ? box.top : box.left
    for (const [index, node] of nodes.entries()) {
      const rect = node.getBoundingClientRect()
      const [lead, size] = vertical ? [rect.top, rect.height] : [rect.left, rect.width]
      if (pointer < lead + size / 2) return { index, offset: lead - start + scroll - 1 }
    }
    const last = nodes.at(-1)?.getBoundingClientRect()
    return {
      index: tabs.length,
      offset: last === undefined ? 0 : (vertical ? last.bottom : last.right) - start + scroll - 1,
    }
  }

  const { dropProps, isDropTarget } = useDrop({
    ref: list,
    getDropOperation: (types) => (types.has(dragType) ? 'move' : 'cancel'),
    onDropMove: (event) => setDrop(dropTarget(event)),
    onDropExit: () => setDrop(null),
    onDrop: async (event) => {
      const target = dropTarget(event)
      setDrop(null)
      const payload = await readTabDrop({ items: event.items, dragType })
      if (payload === null) return
      // Removing the source from the same strip shifts later insertion points back by one.
      const sourceIndex =
        payload.groupId === groupId ? tabs.findIndex((tab) => tab.id === payload.tabId) : -1
      onMove({
        source: payload,
        toIndex: sourceIndex !== -1 && sourceIndex < target.index ? target.index - 1 : target.index,
      })
    },
  })

  const reorder = ({ tab, to }: { readonly tab: EditorTabItem; readonly to: number }) => {
    if (tabs[to]?.id === tab.id) return
    onMove({ source: { groupId, tabId: tab.id }, toIndex: to })
    setAnnouncement(`Moved ${tab.title} to position ${to + 1} of ${tabs.length}`)
  }

  return (
    <Tabs
      {...(activeId === null ? {} : { selectedKey: activeId })}
      onSelectionChange={(key: Key) => onActivate(String(key))}
      orientation={vertical ? 'vertical' : 'horizontal'}
      {...stylex.props(styles.tabs)}
    >
      <div
        {...stylex.props(
          styles.strip,
          variant === 'classic'
            ? styles.stripClassic
            : variant === 'compact'
              ? styles.stripCompact
              : styles.stripVertical,
        )}
      >
        {vertical ? (
          <div {...stylex.props(styles.verticalHeading)}>
            <span>Open surfaces</span>
            <span>{tabs.length}</span>
          </div>
        ) : null}
        <TabList
          ref={list}
          aria-label={label}
          onWheel={(event) => {
            if (!vertical && list.current !== null && event.deltaY !== 0)
              list.current.scrollLeft += event.deltaY
          }}
          {...definedProps(dropProps)}
          {...stylex.props(
            styles.list,
            variant === 'compact' && styles.listCompact,
            vertical && styles.listVertical,
            isDropTarget && styles.listDropTarget,
          )}
        >
          {tabs.map((tab, index) => (
            <TabItem
              key={tab.id}
              tab={tab}
              index={index}
              count={tabs.length}
              variant={variant}
              active={tab.id === activeId}
              isGroupFocused={isGroupFocused}
              groupId={groupId}
              dragType={dragType}
              onClose={onClose}
              onReorder={reorder}
            />
          ))}
        </TabList>
        {drop === null ? null : (
          <span
            aria-hidden="true"
            {...stylex.props(
              vertical ? styles.dropIndicatorY(drop.offset) : styles.dropIndicatorX(drop.offset),
            )}
          />
        )}
        <div {...stylex.props(styles.actions, vertical && styles.actionsVertical)}>
          {actions}
          <MenuTrigger>
            <IconButton label="Open surfaces" icon={<MoreIcon />} />
            <MenuPopover>
              <Menu
                aria-label={`${label}: open surfaces`}
                selectionMode="single"
                selectedKeys={activeId === null ? [] : [activeId]}
                onAction={(key) => onActivate(String(key))}
              >
                {tabs.map((tab) => (
                  <MenuItem
                    key={tab.id}
                    id={tab.id}
                    {...(tab.detail === undefined && tab.harness === undefined
                      ? {}
                      : {
                          suffix: [tab.detail, tab.harness]
                            .filter((part) => part !== undefined)
                            .join(' · '),
                        })}
                  >
                    {tab.title}
                  </MenuItem>
                ))}
              </Menu>
            </MenuPopover>
          </MenuTrigger>
        </div>
        <span role="status" aria-live="polite" {...stylex.props(styles.srOnly)}>
          {announcement}
        </span>
      </div>
      {activeId === null ? (
        children
      ) : (
        <TabPanel key={activeId} id={activeId} {...stylex.props(styles.panel)}>
          {children}
        </TabPanel>
      )}
    </Tabs>
  )
}

interface TabItemProps {
  readonly tab: EditorTabItem
  readonly index: number
  readonly count: number
  readonly variant: EditorTabsVariant
  readonly active: boolean
  readonly isGroupFocused: boolean
  readonly groupId: string
  readonly dragType: string
  readonly onClose: (id: string) => void
  readonly onReorder: (move: { readonly tab: EditorTabItem; readonly to: number }) => void
}

const TabItem = ({
  tab,
  index,
  count,
  variant,
  active,
  isGroupFocused,
  groupId,
  dragType,
  onClose,
  onReorder,
}: TabItemProps) => {
  const [hovered, setHovered] = useState(false)
  const { dragProps, isDragging } = useDrag({
    getItems: () => [
      { [dragType]: JSON.stringify({ groupId, tabId: tab.id }), 'text/plain': tab.title },
    ],
    getAllowedDropOperations: () => ['move'],
  })
  const back = variant === 'vertical' ? 'ArrowUp' : 'ArrowLeft'
  const forward = variant === 'vertical' ? 'ArrowDown' : 'ArrowRight'
  const vertical = variant === 'vertical'
  const detail =
    tab.detail === undefined && tab.harness === undefined ? null : (
      <span {...stylex.props(styles.detail)}>
        {tab.detail}
        {tab.detail !== undefined && tab.harness !== undefined ? ' · ' : null}
        {tab.harness === undefined ? null : <HarnessIcon id={tab.harness} />}
      </span>
    )
  return (
    <Tab
      id={tab.id}
      data-tab-id={tab.id}
      aria-label={`${tab.title}${tab.detail === undefined && tab.harness === undefined ? '' : `, ${[tab.detail, tab.harness].filter((part) => part !== undefined).join(' · ')}`}${tab.attention === true ? ', needs attention' : ''}${tab.status === 'live' ? ', live' : tab.status === 'unavailable' ? ', unavailable' : ''}`}
      aria-keyshortcuts={`Enter Delete Alt+Shift+${back} Alt+Shift+${forward}`}
      onHoverChange={setHovered}
      render={(props) => (
        <div
          {...mergeProps(props, dragProps, {
            onKeyDownCapture: (event: KeyboardEvent) => {
              if (event.altKey && event.shiftKey && (event.key === back || event.key === forward)) {
                onReorder({
                  tab,
                  to: Math.max(0, Math.min(count - 1, index + (event.key === back ? -1 : 1))),
                })
              } else if (event.key === 'Delete' || event.key === 'Backspace') {
                onClose(tab.id)
              } else {
                return
              }
              event.preventDefault()
              event.stopPropagation()
            },
            onAuxClick: (event: MouseEvent) => {
              if (event.button === 1) onClose(tab.id)
            },
          })}
        />
      )}
      className={({ isFocusVisible }) =>
        stylex.props(
          styles.tab,
          variant === 'classic'
            ? styles.tabClassic
            : variant === 'compact'
              ? styles.tabCompact
              : styles.tabVertical,
          active &&
            (variant === 'classic'
              ? styles.tabClassicActive
              : variant === 'compact'
                ? styles.tabCompactActive
                : styles.tabVerticalActive),
          active && variant === 'classic' && isGroupFocused && styles.tabClassicFocused,
          tab.attention === true && styles.attention,
          tab.status === 'unavailable' && styles.unavailable,
          isDragging && styles.dragging,
          isFocusVisible && styles.focusVisible,
        ).className ?? ''
      }
    >
      {() => (
        <>
          {tab.status === 'live' && !vertical ? <span {...stylex.props(styles.live)} /> : null}
          {tab.icon === undefined ? null : <span {...stylex.props(styles.icon)}>{tab.icon}</span>}
          {vertical ? (
            <span {...stylex.props(styles.verticalText)}>
              <span {...stylex.props(styles.title)}>{tab.title}</span>
              {detail}
            </span>
          ) : (
            <>
              <span {...stylex.props(styles.title)}>{tab.title}</span>
              {detail}
            </>
          )}
          {vertical && tab.attention === true ? (
            <span {...stylex.props(styles.attentionDot)} />
          ) : null}
          {vertical && tab.status === 'live' && tab.attention !== true ? (
            <span {...stylex.props(styles.live, styles.liveRing)} />
          ) : null}
          <span
            aria-hidden="true"
            onPointerDown={(event) => event.stopPropagation()}
            onClick={(event) => {
              event.stopPropagation()
              onClose(tab.id)
            }}
            {...stylex.props(
              styles.trailing,
              tab.dirty === true || active || hovered ? styles.closeShown : styles.closeHidden,
            )}
          >
            {tab.dirty === true && !hovered ? (
              <span {...stylex.props(styles.dirtyDot)} />
            ) : (
              <CloseIcon />
            )}
          </span>
        </>
      )}
    </Tab>
  )
}

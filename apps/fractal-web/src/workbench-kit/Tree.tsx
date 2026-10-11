// Navigator tree on React Aria `Tree`: arrow keys move and expand, typeahead, Enter/click runs
// `onAction` (deliberate open). Focus alone never acts (CAG.CLI.TUI-R05). The tree has no selection;
// the row of the active editor's subject is marked `aria-current` instead.

import * as stylex from '@stylexjs/stylex'
import { createContext, useContext, type ReactNode } from 'react'
import {
  Button as AriaButton,
  Tree as AriaTree,
  TreeItem as AriaTreeItem,
  TreeItemContent,
  type Key,
} from 'react-aria-components'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import { ChevronRightIcon } from './icons.tsx'

const styles = stylex.create({
  tree: {
    display: 'flex',
    flexDirection: 'column',
    paddingBlock: scale.space1,
    outlineStyle: 'none',
    fontSize: '0.8125rem',
    lineHeight: '1.25rem',
  },
  item: {
    outlineStyle: 'none',
    color: tokens['--ds-gray-1000'],
    cursor: 'default',
  },
  row: {
    display: 'flex',
    alignItems: 'center',
    gap: '6px',
    height: '1.75rem',
    marginInline: scale.space1,
    paddingInlineEnd: scale.space2,
    borderRadius: scale.radiusSm,
    backgroundColor: 'transparent',
  },
  rowHovered: { backgroundColor: tokens['--ds-gray-alpha-100'] },
  rowSelected: { backgroundColor: tokens['--ds-gray-alpha-200'] },
  rowFocusVisible: { boxShadow: `inset 0 0 0 2px ${tokens['--ds-focus-color']}` },
  indent: (level: number) => ({ paddingInlineStart: `${(level - 1) * 12 + 4}px` }),
  chevron: {
    display: 'inline-flex',
    alignItems: 'center',
    justifyContent: 'center',
    flexShrink: 0,
    width: '1rem',
    height: '1rem',
    padding: 0,
    borderWidth: 0,
    color: tokens['--ds-gray-900'],
    backgroundColor: 'transparent',
    cursor: 'pointer',
    transitionProperty: 'transform',
    transitionDuration: '120ms',
  },
  chevronExpanded: { transform: 'rotate(90deg)' },
  chevronSpacer: { width: '1rem', flexShrink: 0 },
  label: { display: 'flex', alignItems: 'center', gap: '6px', flexGrow: 1, minWidth: 0 },
})

/** Navigator tree label, rows, action handler, and current editor row. */
export interface TreeProps {
  readonly label: string
  readonly children: ReactNode
  readonly onAction?: (id: string) => void
  /** Id of the row whose subject the focused editor shows. */
  readonly currentId?: string | null
  readonly defaultExpandedKeys?: Iterable<Key>
}

const CurrentContext = createContext<string | null>(null)

/** Selection-free navigator tree that marks the focused editor's subject as current. */
export const Tree = ({
  label,
  children,
  onAction,
  currentId = null,
  defaultExpandedKeys,
}: TreeProps) => (
  <CurrentContext.Provider value={currentId}>
    <AriaTree
      aria-label={label}
      // Selection-free: with `selectionMode` none, both click and Enter run `onAction`.
      selectionMode="none"
      {...(defaultExpandedKeys === undefined ? {} : { defaultExpandedKeys })}
      {...(onAction === undefined ? {} : { onAction: (key: Key) => onAction(String(key)) })}
      {...stylex.props(styles.tree)}
    >
      {children}
    </AriaTree>
  </CurrentContext.Provider>
)

/** Navigator row identity, typeahead text, content, and nested rows. */
export interface TreeNodeProps {
  readonly id: string
  readonly textValue: string
  readonly content: ReactNode
  readonly children?: ReactNode
  readonly onHoverStart?: () => void
  readonly onFocus?: () => void
}

/** Navigator row with disclosure affordance and current-editor marker. */
export const TreeNode = ({
  id,
  textValue,
  content,
  children,
  onHoverStart,
  onFocus,
}: TreeNodeProps) => {
  const current = useContext(CurrentContext) === id
  return (
    <AriaTreeItem
      key={id}
      id={id}
      textValue={textValue}
      {...(onHoverStart === undefined ? {} : { onHoverStart })}
      {...(onFocus === undefined ? {} : { onFocus })}
      // TreeItem filters aria-current; its forwarded ref is the accessible row.
      ref={(row) => {
        if (current) row?.setAttribute('aria-current', 'true')
        else row?.removeAttribute('aria-current')
      }}
      {...stylex.props(styles.item)}
    >
      <TreeItemContent>
        {({ hasChildItems, isExpanded, level, isHovered, isFocusVisible }) => (
          <div
            {...stylex.props(
              styles.row,
              styles.indent(level),
              isHovered && styles.rowHovered,
              current && styles.rowSelected,
              isFocusVisible && styles.rowFocusVisible,
            )}
          >
            {hasChildItems ? (
              <AriaButton
                slot="chevron"
                {...stylex.props(styles.chevron, isExpanded && styles.chevronExpanded)}
              >
                <ChevronRightIcon />
              </AriaButton>
            ) : (
              <span {...stylex.props(styles.chevronSpacer)} />
            )}
            <span {...stylex.props(styles.label)}>{content}</span>
          </div>
        )}
      </TreeItemContent>
      {children}
    </AriaTreeItem>
  )
}

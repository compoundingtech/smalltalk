import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import {
  Button,
  ButtonContext,
  GridList,
  GridListItem,
  ListLayout,
  Virtualizer,
} from 'react-aria-components'

import { ScrollController } from './embrace-virtual/ScrollController'
import type { FollowState } from './embrace-virtual/FollowController'
import { accentTokens, tokens } from './embrace-tokens.stylex'

export interface EmbraceVirtualConversationProps<T extends { readonly id: string }> {
  readonly items: readonly T[]
  readonly renderItem: (item: T, index: number) => React.ReactNode
  /** Applied to the actual scroll element, not the outer frame. */
  readonly className?: string
  readonly style?: React.CSSProperties
  /** Source transcript's initial estimate; measured heights remain authoritative. */
  readonly estimatedRowHeight?: number
  /** Changing conversations opens at the live edge. */
  readonly anchorKey?: string
  /** A changed own-send key resumes following. */
  readonly scrollToBottomKey?: string
  /** @deprecated Accepted during the host migration; conversation opens no longer restore old anchors. */
  readonly persistAnchor?: boolean
}

/**
 * Existing RAC variable-height transcript virtualization with an interchangeable
 * renderer. The story can supply the same assistant-ui message boundary to this
 * component and to ThreadPrimitive.Viewport without changing the fixture.
 */
export function EmbraceVirtualConversation<T extends { readonly id: string }>(
  props: EmbraceVirtualConversationProps<T>,
) {
  return <VirtualConversationBody key={props.anchorKey ?? 'story'} {...props} />
}

function VirtualConversationBody<T extends { readonly id: string }>({
  items,
  renderItem,
  className,
  style,
  estimatedRowHeight = 160,
  scrollToBottomKey,
}: EmbraceVirtualConversationProps<T>) {
  // Keep the actual instance so the source controller reads public row geometry.
  const [layout] = React.useState(() => new ListLayout())
  const [followState, setFollowState] = React.useState<FollowState>({ _tag: 'Attached' })
  const [scroll] = React.useState(() => new ScrollController({ layout, onStateChange: setFollowState }))
  const previousCommand = React.useRef(scrollToBottomKey)
  React.useLayoutEffect(() => {
    if (scrollToBottomKey !== undefined && scrollToBottomKey !== previousCommand.current) scroll.jump()
    previousCommand.current = scrollToBottomKey
  }, [scroll, scrollToBottomKey])
  React.useLayoutEffect(() => scroll.afterRowsChange(items), [scroll, items])
  const layoutOptions = React.useMemo(() => ({ estimatedRowHeight }), [estimatedRowHeight])
  const indexById = React.useMemo(() => {
    const indices = new Map<string, number>()
    items.forEach((item, index) => indices.set(item.id, index))
    return indices
  }, [items])
  const renderRow = React.useCallback((item: T) => (
    <GridListItem id={item.id} textValue={item.id} {...stylex.props(styles.row)}>
      <Entry item={item} index={indexById.get(item.id)!} renderItem={renderItem} />
    </GridListItem>
  ), [indexById, renderItem])
  const listStyles = stylex.props(styles.list)

  return (
    <div role="region" aria-label="Conversation" tabIndex={-1} {...stylex.props(styles.frame)}>
      <Virtualizer layout={layout} layoutOptions={layoutOptions} shouldObserveItemSize>
        <GridList
          ref={scroll.attach}
          aria-label="Conversation"
          items={items}
          dependencies={[indexById, renderItem]}
          keyboardNavigationBehavior="tab"
          data-testid="transcript-scroll"
          data-perf-target="conversation-list"
          {...listStyles}
          className={[listStyles.className, className].filter(Boolean).join(' ')}
          style={style}
        >
          {renderRow}
        </GridList>
      </Virtualizer>
      <span role="status" aria-live="polite" {...stylex.props(styles.announcement)}>{followState._tag === 'Detached' ? 'Reading earlier messages. Jump to latest is available.' : ''}</span>
      {followState._tag === 'Detached' ? (
        <div {...stylex.props(styles.jump)}>
          <Button onPress={scroll.jump} {...stylex.props(styles.jumpButton)}>
            New messages ↓
          </Button>
        </div>
      ) : null}
    </div>
  )
}

/** Like source BlockRow, the renderer runs only when RAC mounts a visible row. */
function Entry<T extends { readonly id: string }>({
  item,
  index,
  renderItem,
}: {
  readonly item: T
  readonly index: number
  readonly renderItem: (item: T, index: number) => React.ReactNode
}) {
  return (
    <div data-embrace-entry-id={item.id} {...stylex.props(styles.entry)}>
      {/* Message actions belong to the renderer, not RAC drag/action slots. */}
      <ButtonContext.Provider value={null}>
        {renderItem(item, index)}
      </ButtonContext.Provider>
    </div>
  )
}

const styles = stylex.create({
  frame: {
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minHeight: 0,
    height: '100%',
    position: 'relative',
  },
  list: {
    flexGrow: 1,
    minHeight: 0,
    overflowY: 'auto',
    overflowAnchor: 'none',
    outlineStyle: 'none',
    backgroundColor: tokens.canvas,
  },
  row: {
    outlineStyle: 'none',
    boxShadow: { default: null, ':focus-visible': `inset 0 0 0 2px ${accentTokens.accent}` },
  },
  entry: {
    display: 'flow-root',
    minWidth: 0,
  },
  announcement: { position: 'absolute', width: 1, height: 1, overflow: 'hidden', clipPath: 'inset(50%)', whiteSpace: 'nowrap' },
  jump: { flexShrink: 0, marginBlock: '12px', alignSelf: 'center' },
  jumpButton: {
    minHeight: '28px',
    paddingInline: '12px',
    borderWidth: 1,
    borderStyle: 'solid',
    borderColor: tokens.line,
    borderRadius: '4px',
    backgroundColor: { default: tokens.panel, ':hover': tokens.selection },
    color: tokens.ink,
    fontFamily: 'inherit',
    fontSize: '12px',
    cursor: 'pointer',
    outlineColor: accentTokens.accent,
    ':focus-visible': { outlineWidth: 2, outlineStyle: 'solid', outlineOffset: 2 },
  },
})

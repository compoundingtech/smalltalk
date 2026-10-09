import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import {
  ButtonContext,
  GridList,
  GridListItem,
  ListLayout,
  Virtualizer,
} from 'react-aria-components'

import { ScrollController } from './embrace-virtual/ScrollController'
import { FollowAffordance } from './embrace-virtual/FollowAffordance'
import { ViewportStoreContext } from './EmbraceScrollViewport'
import { accentTokens, tokens } from './embrace-tokens.stylex'

export interface EmbraceVirtualConversationProps<T extends { readonly id: string }> {
  readonly items: readonly T[]
  readonly renderItem: (item: T, index: number) => React.ReactNode
  /** Applied to the actual scroll element, not the outer frame. */
  readonly className?: string
  readonly style?: React.CSSProperties
  /** Source transcript's initial estimate; measured heights remain authoritative. */
  readonly estimatedRowHeight?: number
  /** Conversation identity; an enclosing ViewportStore restores in-app reading positions. */
  readonly anchorKey?: string
  /** A changed own-send key resumes following. */
  readonly scrollToBottomKey?: string
  /** @deprecated Local-storage restoration is removed; use the surface's in-memory ViewportStoreContext. */
  readonly persistAnchor?: boolean
  readonly isRunning?: boolean
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
  anchorKey,
  isRunning = false,
}: EmbraceVirtualConversationProps<T>) {
  const store = React.useContext(ViewportStoreContext)
  // Keep the actual instance so the source controller reads public row geometry.
  const [layout] = React.useState(() => new ListLayout())
  const [jumpVisible, setJumpVisible] = React.useState(false)
  const [scroll] = React.useState(() => new ScrollController({ layout, saved: anchorKey === undefined ? undefined : store?.get(anchorKey), onVisibilityChange: setJumpVisible }))
  const previousCommand = React.useRef(scrollToBottomKey)
  React.useLayoutEffect(() => {
    scroll.bindStore(store, anchorKey)
    scroll.setRunning(isRunning)
    if (scrollToBottomKey !== undefined && scrollToBottomKey !== previousCommand.current) scroll.jump()
    previousCommand.current = scrollToBottomKey
  }, [scroll, scrollToBottomKey, isRunning, store, anchorKey])
  React.useLayoutEffect(() => scroll.afterRowsChange(items), [scroll, items])
  React.useLayoutEffect(() => () => {
    if (anchorKey !== undefined) store?.save(anchorKey, scroll.released())
  }, [scroll, anchorKey, store])
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
      <span role="status" aria-live="polite" {...stylex.props(styles.announcement)}>{jumpVisible ? 'Reading earlier messages. Scroll to end is available.' : ''}</span>
      {jumpVisible ? <FollowAffordance hidden={false} onPress={scroll.activate} /> : null}
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
})

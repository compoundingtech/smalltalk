// Explicit-clock presentation only.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Icon } from '../composition/Icons'
import { Button, Dialog, DialogTrigger, Menu, MenuItem, MenuTrigger, Popover, Tooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import { accentVars as accent, borderVars as border, elevationVars as elevation, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as tone, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { matchPositions, type SidebarActions, type SidebarAgentRow as Row } from './model'
import { SidebarStatus, type DiscRefinement, type GlyphVariant } from './SidebarStatus'
import { SidebarTime, type SidebarClock } from './SidebarTime'
import { SidebarDuration } from './SidebarDuration'
import { SidebarRowSignals, attachSidebarLine1Fit } from './SidebarRowSignals'

export type RowVariant = 'SR-1' | 'SR-2' | 'SR-3'
export type RowLayout = 'SR2-A' | 'SR2-B' | 'SR2-C'
export type ExtraSignal = 'X-needs' | 'X-unread' | 'X-work' | 'X-model' | 'X-pr' | 'X-subs'
export const defaultExtraSignals: readonly ExtraSignal[] = ['X-needs', 'X-unread', 'X-work', 'X-subs']
const tokenCount = Intl.NumberFormat('en', { notation: 'compact', maximumSignificantDigits: 2 })
const spendCount = Intl.NumberFormat('en', { style: 'currency', currency: 'USD', notation: 'compact', maximumSignificantDigits: 2 })
export const rowDescriptions: Record<RowVariant, string> = { 'SR-1': '32px compact row; complete reported facts in accessible details', 'SR-2': '48px shared two-line layouts A/B/C; B shows current work and retains spend in accessible details', 'SR-3': '68px three-line row with extra work metadata; complete details remain accessible' }
export type SidebarAgentRowProps = SidebarClock & {
  readonly item: Row
  readonly variant?: RowVariant
  readonly layout?: RowLayout
  readonly glyph?: GlyphVariant
  readonly extraSignals?: readonly ExtraSignal[]
  readonly discRefinement?: DiscRefinement
  readonly active?: boolean
  readonly query?: string
  readonly actions?: SidebarActions
  readonly onOpen?: (row: Row) => void
  readonly onMarkRead?: (ref: string) => void
  readonly onToggleChildren?: (ref: string) => void
  readonly inTree?: boolean
  readonly treeExpanded?: boolean
}
/** Fuzzy highlighting uses the upstream matcher, not a second search implementation. */
export function Highlight({ value, query = '' }: { readonly value: string; readonly query?: string }) {
  const matched = matchPositions({ query, text: value })
  if (matched.size === 0) return <>{value}</>
  return <>{Array.from(value, (letter, index) => matched.has(index) ? <mark key={index} {...stylex.props(styles.mark)}>{letter}</mark> : letter)}</>
}
function usageText(usage: Extract<Row['usage'], { _tag: 'Known' }>): string {
  return `$${usage.usd} / ${usage.tokens} tokens; ${usage.scope === '24h-root-and-subagents' ? '24h root and subagents' : 'lifetime including subagents'}`
}
export const sidebarRowTitle = (row: Row): string => row.title
/** Reported metadata stays accessible; fields the source never reported are omitted, never described as unknown. */
export function sidebarRowDescription(row: Row): string {
  const parts: string[] = [row.title]
  if (row.description !== undefined) parts.push(row.description)
  parts.push(`Host: ${row.host}. Status: ${row.statusLabel}; ${row.freshness} observation`)
  if (row.usage._tag === 'Known') parts.push(usageText(row.usage))
  if (row.duration._tag === 'Known') parts.push(`${row.duration.scope === '24h-activity-span' ? '24h activity span' : 'Lifetime duration'}: ${row.duration.ms} milliseconds`)
  if (row.lastTurn._tag === 'Known') parts.push(`${row.lastTurn.kind === 'activity' ? 'Since activity' : 'Last completed turn'}: ${new Date(row.lastTurn.at).toISOString()}`)
  if (row.harness !== undefined) parts.push(`Harness: ${row.harness}`)
  if (row.model !== undefined) parts.push(`Model: ${row.model}`)
  if (row.needsMe !== undefined) parts.push(row.needsMe ? 'Needs your attention' : 'No attention requested')
  if (row.unread !== undefined) parts.push(`Unread: ${row.unread}`)
  if (row.statusSince !== undefined) parts.push(`Status boundary: ${new Date(row.statusSince).toISOString()}`)
  if (row.lastActivityAt !== undefined) parts.push(`Last activity: ${new Date(row.lastActivityAt).toISOString()}`)
  if (row.branch !== undefined) parts.push(`Branch: ${row.branch}`)
  if (row.worktree !== undefined) parts.push(`Worktree: ${row.worktree}`)
  if (row.pullRequest !== undefined) parts.push(`Pull request: #${row.pullRequest.number}, ${row.pullRequest.title}, ${row.pullRequest.state}, ${row.pullRequest.ref}`)
  if (row.terminal !== undefined) parts.push(`Terminal: ${row.terminal}`)
  if (row.mission !== undefined) parts.push(`Mission: ${row.mission}`)
  if (row.childrenKnown !== false) parts.push(`Subagents: ${row.children.length}`)
  parts.push(`ID: ${row.ref}`)
  return `${parts.join('. ')}.`
}
/** Complete reported facts stay accessible; unreported fields are omitted, never shown as placeholders. */
export function AgentRowDetails({ row, ...clock }: { readonly row: Row } & SidebarClock) {
  return <dl {...stylex.props(styles.details)}>
    <dt {...stylex.props(styles.detailLabel)}>Name</dt><dd {...stylex.props(styles.detailValue)}>{sidebarRowTitle(row)}</dd>
    <dt {...stylex.props(styles.detailLabel)}>ID</dt><dd {...stylex.props(styles.detailValue)}>{row.ref}</dd>
    {row.description !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Description</dt><dd {...stylex.props(styles.detailValue)}>{row.description}</dd></>}
    <dt {...stylex.props(styles.detailLabel)}>Status</dt><dd {...stylex.props(styles.detailValue)}>{row.statusLabel}; {row.freshness} observation</dd>
    {row.statusSince !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Elapsed in status</dt><dd {...stylex.props(styles.detailValue)}><SidebarTime at={row.statusSince} {...clock} />; since {new Date(row.statusSince).toISOString()}</dd></>}
    {row.lastActivityAt !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Last activity</dt><dd {...stylex.props(styles.detailValue)}><SidebarTime at={row.lastActivityAt} {...clock} kind="activity" />; {new Date(row.lastActivityAt).toISOString()}</dd></>}
    {row.needsMe !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Needs you</dt><dd {...stylex.props(styles.detailValue)}>{row.needsMe ? 'Yes' : 'No'}</dd></>}
    {row.unread !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Unread</dt><dd {...stylex.props(styles.detailValue)}>{row.unread}</dd></>}
    <dt {...stylex.props(styles.detailLabel)}>Host</dt><dd {...stylex.props(styles.detailValue)}>{row.host}</dd>
    {row.harness !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Harness</dt><dd {...stylex.props(styles.detailValue)}>{row.harness}</dd></>}
    {row.model !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Model</dt><dd {...stylex.props(styles.detailValue)}>{row.model}</dd></>}
    {row.usage._tag === 'Known' && <><dt {...stylex.props(styles.detailLabel)}>Usage</dt><dd {...stylex.props(styles.detailValue)}>{usageText(row.usage)}</dd></>}
    {row.duration._tag === 'Known' && <><dt {...stylex.props(styles.detailLabel)}>Duration</dt><dd {...stylex.props(styles.detailValue)}><SidebarDuration row={row} /></dd></>}
    {row.lastTurn._tag === 'Known' && <><dt {...stylex.props(styles.detailLabel)}>Last turn or activity</dt><dd {...stylex.props(styles.detailValue)}><SidebarTime at={row.lastTurn.at} {...clock} kind={row.lastTurn.kind === 'turn-completed' ? 'turn' : 'activity'} showKind /></dd></>}
    {row.branch !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Branch</dt><dd {...stylex.props(styles.detailValue)}>{row.branch}</dd></>}
    {row.worktree !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Worktree</dt><dd {...stylex.props(styles.detailValue)}>{row.worktree}</dd></>}
    {row.pullRequest !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Pull request</dt><dd {...stylex.props(styles.detailValue)}>#{row.pullRequest.number}: {row.pullRequest.title}; {row.pullRequest.state}; {row.pullRequest.ref}</dd></>}
    {row.terminal !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Terminal</dt><dd {...stylex.props(styles.detailValue)}>{row.terminal}</dd></>}
    {row.mission !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Mission</dt><dd {...stylex.props(styles.detailValue)}>{row.mission}</dd></>}
    {row.subagent !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Subagent ID</dt><dd {...stylex.props(styles.detailValue)}>{row.subagent.id}</dd>
      {row.subagent.subagent_type !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Kind</dt><dd {...stylex.props(styles.detailValue)}>{row.subagent.subagent_type}</dd></>}
      {row.subagent.work_id !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Work</dt><dd {...stylex.props(styles.detailValue)}>{row.subagent.work_id}</dd></>}
      {row.subagent.session_id !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Session</dt><dd {...stylex.props(styles.detailValue)}>{row.subagent.session_id}</dd></>}
      {row.subagent.started_at !== undefined && <><dt {...stylex.props(styles.detailLabel)}>Started</dt><dd {...stylex.props(styles.detailValue)}><time dateTime={row.subagent.started_at}>{row.subagent.started_at}</time></dd></>}
      <dt {...stylex.props(styles.detailLabel)}>Lease expires</dt><dd {...stylex.props(styles.detailValue)}><time dateTime={row.subagent.lease_expires_at}>{row.subagent.lease_expires_at}</time></dd></>}
    <dt {...stylex.props(styles.detailLabel)}>Subagents</dt><dd {...stylex.props(styles.detailValue)}>{row.childrenKnown === false ? 'Subagent observations unavailable' : `${row.children.length} collapsed children`}{row.children.length > 0 && <ul>{row.children.map(child => <li key={child.ref}>{[sidebarRowTitle(child), child.ref, ...(child.subagent !== undefined ? [child.subagent.subagent_type, child.subagent.work_id, `lease ${child.subagent.lease_expires_at}`] : [])].filter((part): part is string => part !== undefined).join('; ')}</li>)}</ul>}</dd>
  </dl>
}
/** Only reported facts render; unknown or unavailable fields are omitted rather than shown as placeholders. */
export function AgentHoverCard({ row, ...clock }: { readonly row: Row } & SidebarClock) {
  return <div data-testid="agent-hover-card" {...stylex.props(styles.card)}>
    <strong {...stylex.props(styles.cardTitle)}>{row.title}</strong>
    <div {...stylex.props(styles.cardSummary)}><SidebarStatus status={row.status} statusLabel={row.statusLabel} variant="SG-1" freshness={row.freshness} iconOnly /><span>{row.statusLabel}</span><span>·</span><Icon name="gear" size={12} /><span>{row.host}</span></div>
    {row.description !== undefined && <p {...stylex.props(styles.cardWork)}><Icon name="message" size={12} />{row.description}</p>}
    <dl {...stylex.props(styles.details)}>
      {row.usage._tag === 'Known' && <><dt {...stylex.props(styles.cardLabel)}><Icon name="dot" size={12} />Spend</dt><dd {...stylex.props(styles.detailValue)}>{spendCount.format(row.usage.usd)} · {tokenCount.format(row.usage.tokens)} tokens<div {...stylex.props(styles.cardScope)}>{row.usage.scope === '24h-root-and-subagents' ? '24h · includes subagents' : 'Lifetime · includes subagents'}</div></dd></>}
      {row.duration._tag === 'Known' && <><dt {...stylex.props(styles.cardLabel)}><Icon name="clock" size={12} />Duration</dt><dd {...stylex.props(styles.detailValue)}><SidebarDuration row={row} /></dd></>}
      {row.lastTurn._tag === 'Known' && <><dt {...stylex.props(styles.cardLabel)}><Icon name="clock" size={12} />{row.lastTurn.kind === 'turn-completed' ? 'Last turn' : 'Last activity'}</dt><dd {...stylex.props(styles.detailValue)}><SidebarTime at={row.lastTurn.at} {...clock} kind={row.lastTurn.kind === 'turn-completed' ? 'turn' : 'activity'} /></dd></>}
      {row.model !== undefined && <><dt {...stylex.props(styles.cardLabel)}><Icon name="gear" size={12} />Model</dt><dd {...stylex.props(styles.detailValue)}>{row.model}</dd></>}
      {row.pullRequest !== undefined && <><dt {...stylex.props(styles.cardLabel)}><Icon name="swap" size={12} />PR</dt><dd {...stylex.props(styles.detailValue)}>#{row.pullRequest.number} · {row.pullRequest.title}</dd></>}
      {row.branch !== undefined && <><dt {...stylex.props(styles.cardLabel)}><Icon name="swap" size={12} />Branch</dt><dd {...stylex.props(styles.detailValue)}>{row.branch}</dd></>}
      {row.children.length > 0 && <><dt {...stylex.props(styles.cardLabel)}><Icon name="message" size={12} />Subagents</dt><dd {...stylex.props(styles.detailValue)}>{row.children.length}<ul {...stylex.props(styles.cardChildren)}>{row.children.slice(0, 3).map(child => <li key={child.ref}>{sidebarRowTitle(child)}</li>)}</ul></dd></>}
    </dl>
  </div>
}
export const SidebarAgentRow = React.memo(function SidebarAgentRow({ item, variant = 'SR-2', layout = 'SR2-A', glyph = 'SG-1', extraSignals = defaultExtraSignals, discRefinement = 'G1', active = false, query = '', now, actions, onOpen, onMarkRead, onToggleChildren, inTree = false, treeExpanded }: SidebarAgentRowProps) {
  const title = sidebarRowTitle(item)
  const host = item.host
  const metadata = React.useMemo(() => sidebarRowDescription(item), [item])
  const descriptionId = React.useId()
  const ownerId = item.parentRef?.replace(/^agent\//, '') ?? item.id
  const clock: SidebarClock = { now: now! }
  const callbacks = actions
  const usd = item.usage._tag === 'Known' ? item.usage.usd : undefined
  const tokens = item.usage._tag === 'Known' ? item.usage.tokens : undefined
  const usageScope = item.usage._tag === 'Known' && item.usage.scope === '24h-root-and-subagents' ? '24h root and subagents' : 'Lifetime including subagents'
  const expanded = treeExpanded ?? false
  const canOpen = callbacks?.select !== undefined || onOpen !== undefined
  const rowLayout = variant === 'SR-2' ? layout : 'SR2-A'
  const rowGlyph = variant === 'SR-2' ? 'SG-1' : glyph
  const menuTrigger = React.useRef<HTMLButtonElement>(null)
  const detailsTrigger = React.useRef<HTMLButtonElement>(null)
  const tooltipAnchor = React.useRef<HTMLDivElement>(null)
  const [hoverOpen, setHoverOpen] = React.useState(false)
  const [rowHovered, setRowHovered] = React.useState(false)
  const hoverTimer = React.useRef<number | undefined>(undefined)
  const hoverLifetime = React.useCallback((node: HTMLDivElement | null) => {
    if (node === null) return
    return () => { clearTimeout(hoverTimer.current) }
  }, [])
  // The hover card anchors to the measured row frame so it follows the row, not its hidden trigger.
  const rowAnchor = React.useCallback((node: HTMLDivElement | null) => { attachSidebarLine1Fit(node); tooltipAnchor.current = node }, [])
  const hoverIntent = (open: boolean) => { clearTimeout(hoverTimer.current); hoverTimer.current = window.setTimeout(() => setHoverOpen(open), open ? 150 : 300) }
  const prefetch = () => { if (!inTree) callbacks?.prefetch?.(item.ref) }
  const open = () => { callbacks?.select?.(item.id, item.parentRef?.replace(/^agent\//, '')); onOpen?.(item) }
  const quickOpen = canOpen && rowHovered
  const spendFields = item.usage._tag === 'Unknown' ? null : <span {...stylex.props(styles.metric)}><span data-row-field="total-usd" title={`${usageScope}: $${usd}`} aria-label={`${usageScope}: $${usd}`}>{spendCount.format(item.usage.usd)}</span><span data-row-field="total-tokens" data-line1-drop="tokens" title={`${usageScope}: ${tokens} tokens`} aria-label={`${usageScope}: ${tokens} tokens`}>{tokenCount.format(item.usage.tokens)}</span></span>
  const durationField = item.duration._tag === 'Unknown' ? null : <span data-row-field="total-duration"><SidebarDuration row={item} /></span>
  const lastTurnField = item.lastTurn._tag === 'Unknown' ? null : <span data-row-field="last-turn"><SidebarTime at={item.lastTurn.at} {...clock} kind={item.lastTurn.kind === 'turn-completed' ? 'turn' : 'activity'} compact /></span>
  const content = <>
    <span data-row-column="title" {...stylex.props(styles.title, variant === 'SR-2' && item.children.length > 0 && styles.hasChildren)} title={metadata}>{rowGlyph === 'SG-2' && <span {...stylex.props(styles.statusWord)}>{item.statusLabel}</span>}<span data-row-column="title-text" {...stylex.props(styles.name)}><Highlight value={title} query={query} /></span></span>
    {variant !== 'SR-1' && <SidebarRowSignals>
      {rowLayout === 'SR2-C' && <span data-row-field="status-label" data-row-retention="7" title={item.statusLabel}>{item.statusLabel}</span>}
      {extraSignals.includes('X-work') && item.description !== undefined && <span data-row-field="current-work" data-row-signal="X-work" data-row-retention="2" title={item.description}>{item.description}</span>}
      <span data-row-field="host" data-row-retention="3" title={`Host: ${host}`} aria-label={`Host ${host}`} aria-description={metadata}>{host}</span>
      {rowLayout === 'SR2-A' && <span data-row-retention="5">{spendFields}</span>}
      {rowLayout !== 'SR2-B' && <span data-row-retention="4">{durationField}</span>}
      {rowLayout !== 'SR2-A' && <span data-row-retention="6">{lastTurnField}</span>}
      {extraSignals.includes('X-model') && (item.model ?? item.harness) !== undefined && <span data-row-signal="X-model" data-row-retention="1" title={[item.harness === undefined ? undefined : `Harness: ${item.harness}`, item.model === undefined ? undefined : `model: ${item.model}`].filter(part => part !== undefined).join('; ')}>{item.model ?? item.harness}</span>}
      {extraSignals.includes('X-pr') && (item.pullRequest ?? item.branch ?? item.worktree) !== undefined && <span data-row-signal="X-pr" data-row-retention="0" title={[item.worktree === undefined ? undefined : `Worktree: ${item.worktree}`, item.branch === undefined ? undefined : `branch: ${item.branch}`, item.pullRequest === undefined ? undefined : `pull request: #${item.pullRequest.number}: ${item.pullRequest.title}`].filter(part => part !== undefined).join('; ')}>{item.pullRequest === undefined ? item.branch ?? item.worktree : `#${item.pullRequest.number}`}</span>}
    </SidebarRowSignals>}
    <span {...stylex.props(styles.metadata, variant !== 'SR-3' && styles.hidden)}>{[item.description, item.harness, item.model, item.branch, item.worktree, item.pullRequest === undefined ? undefined : `PR #${item.pullRequest.number}`].filter((part): part is string => part !== undefined).join(' / ')}</span>
  </>
  const rowStyle = stylex.props(styles.row)
  return <div ref={hoverLifetime} data-observation-freshness={item.freshness} data-wf-agent-ref={inTree ? undefined : item.ref} {...stylex.props(styles.wrapper)} onMouseEnter={() => { setRowHovered(true); prefetch(); hoverIntent(true) }} onMouseLeave={() => { setRowHovered(false); hoverIntent(false) } } onFocusCapture={() => { setRowHovered(true); prefetch(); hoverIntent(true) }} onBlurCapture={event => { if (!event.currentTarget.contains(event.relatedTarget)) { setRowHovered(false); hoverIntent(false) } }} onContextMenu={event => { event.preventDefault(); event.stopPropagation(); clearTimeout(hoverTimer.current); setHoverOpen(false); menuTrigger.current?.click() }} onKeyDown={event => { if (event.key === 'Escape') { clearTimeout(hoverTimer.current); setHoverOpen(false) }; if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) { event.preventDefault(); event.stopPropagation(); clearTimeout(hoverTimer.current); setHoverOpen(false); menuTrigger.current?.click() } }}>
    <div ref={rowAnchor} data-testid="taste-agent-row" data-density={variant} data-layout={rowLayout} data-glyph={rowGlyph} data-disc-refinement={discRefinement} data-extra-signals={extraSignals.join(' ')} data-needs-me={item.needsMe} data-unknown-row-fields={[...(item.usage._tag === 'Unknown' ? ['total-usd', 'total-tokens'] : []), ...(item.duration._tag === 'Unknown' ? ['total-duration'] : []), ...(item.lastTurn._tag === 'Unknown' ? ['last-turn'] : []), ...(item.description === undefined ? ['current-work'] : [])].join(' ')} data-required-row-fields={variant === 'SR-2' ? 'true' : undefined} aria-description={metadata} {...stylex.props(styles.rowWrap, variant === 'SR-2' ? item.children.length === 0 ? styles.rowWrapLeaf : styles.rowWrapParent : undefined, styles[variant], active && styles.active)}>
      <span data-row-column="status" data-row-field="status" {...stylex.props(styles.glyph)}><SidebarStatus status={item.status} statusLabel={item.statusLabel} statusSince={item.statusSince} now={clock.now} variant={rowGlyph} discRefinement={discRefinement} freshness={item.freshness} iconOnly /></span>
      <span data-row-trailing-signals {...stylex.props(styles.trailingSignals)}>{extraSignals.includes('X-unread') && (item.unread ?? 0) > 0 && <span data-row-signal="X-unread" aria-label={`${item.unread} unread`} {...stylex.props(styles.unread)}>{item.unread}</span>}{extraSignals.includes('X-needs') && item.needsMe && <span data-row-signal="X-needs" data-row-column="attention" title="Needs your attention" aria-label="Needs your attention" {...stylex.props(styles.needsYou)}>!</span>}</span>
      <span data-row-column="time" data-quick-open={quickOpen ? 'true' : undefined} {...stylex.props(styles.statusTime, quickOpen && styles.timeCovered)}>{rowLayout === 'SR2-A' ? lastTurnField : rowLayout === 'SR2-C' ? spendFields : durationField}{quickOpen && <Button data-row-action="open" aria-label={`Open ${title}`} onPress={open} {...stylex.props(styles.quickOpen)}><Icon name="message" size={12} /></Button>}</span>
      {/* The button's name is its visible content; React Aria drops aria-description, so reported facts describe it by reference. */}
      {inTree ? <div aria-current={active ? 'page' : undefined} aria-description={metadata} {...rowStyle}>{content}</div> : <><Button aria-describedby={descriptionId} aria-current={active ? 'page' : undefined} isDisabled={!canOpen} onPress={open} {...rowStyle}>{content}</Button><span id={descriptionId} hidden>{metadata}</span></>}
      <div {...stylex.props(styles.actions)}>
        <DialogTrigger><TooltipTrigger delay={150} closeDelay={300} isOpen={hoverOpen} onOpenChange={setHoverOpen}><VisuallyHidden isFocusable><Button ref={detailsTrigger} aria-label={`Reported details for ${title}`} onPress={() => setHoverOpen(false)} {...stylex.props(styles.iconButton)}><Icon name="message" /></Button></VisuallyHidden><Tooltip triggerRef={tooltipAnchor} placement="right" {...stylex.props(styles.hovercard)}><AgentHoverCard row={item} {...clock} /></Tooltip></TooltipTrigger><Popover triggerRef={tooltipAnchor} placement="right" {...stylex.props(styles.hovercard)}><Dialog aria-label={`Reported details for ${title}`}><AgentRowDetails row={item} {...clock} /></Dialog></Popover></DialogTrigger>
        <MenuTrigger><Button ref={menuTrigger} data-agent-menu="true" aria-label={`Actions for ${title}`} className={({ isHovered, isPressed, isFocusVisible }) => stylex.props(styles.iconButton, styles.menuButton, (hoverOpen || isHovered || isPressed || isFocusVisible) && styles.menuVisible, variant === 'SR-1' && styles.menuSingle).className ?? ''}><Icon name="gear" /></Button><Popover placement="bottom end" {...stylex.props(styles.popover)}><Menu aria-label={`Actions for ${title}`} {...stylex.props(styles.menu)}>
          <MenuItem id="open" isDisabled={!canOpen} onAction={open} {...stylex.props(styles.menuItem)}>Open conversation</MenuItem>
          {callbacks?.split !== undefined && <MenuItem id="split-right" onAction={() => callbacks.split?.(item.ref, 'right')} {...stylex.props(styles.menuItem)}>Open in split right</MenuItem>}
          {callbacks?.split !== undefined && <MenuItem id="split-below" onAction={() => callbacks.split?.(item.ref, 'below')} {...stylex.props(styles.menuItem)}>Open in split below</MenuItem>}
          <MenuItem id="reported-details" onAction={() => { requestAnimationFrame(() => detailsTrigger.current?.click()) }} {...stylex.props(styles.menuItem)}>Reported agent details</MenuItem>
          {callbacks?.details !== undefined && <MenuItem id="details" onAction={() => callbacks.details?.(item.ref)} {...stylex.props(styles.menuItem)}>Agent details</MenuItem>}
          {(callbacks?.terminal !== undefined || callbacks?.resource !== undefined) && item.terminal !== undefined && <MenuItem id="terminal" onAction={() => callbacks?.terminal !== undefined ? callbacks.terminal(item.terminal!, ownerId) : callbacks?.resource?.(item.terminal!, ownerId)} {...stylex.props(styles.menuItem)}>Open terminal</MenuItem>}
          {callbacks?.resource !== undefined && item.mission !== undefined && <MenuItem id="mission" onAction={() => callbacks.resource?.(item.mission!, ownerId)} {...stylex.props(styles.menuItem)}>Open mission</MenuItem>}
          {callbacks?.resource !== undefined && item.worktree !== undefined && <MenuItem id="worktree" onAction={() => callbacks.resource?.(item.worktree!, ownerId)} {...stylex.props(styles.menuItem)}>Open worktree</MenuItem>}
          {callbacks?.resource !== undefined && item.pullRequest !== undefined && <MenuItem id="pr" onAction={() => callbacks.resource?.(item.pullRequest!.ref, ownerId)} {...stylex.props(styles.menuItem)}>Open pull request</MenuItem>}
          {(item.unread ?? 0) > 0 && onMarkRead !== undefined && <MenuItem id="read" onAction={() => onMarkRead?.(item.ref)} {...stylex.props(styles.menuItem)}>Mark read</MenuItem>}
        </Menu></Popover></MenuTrigger>
        {item.children.length > 0 && <Button data-row-column="children" slot={inTree ? 'chevron' : undefined} isDisabled={!inTree && onToggleChildren === undefined} aria-labelledby="" aria-label={`${expanded ? 'Collapse' : 'Expand'} ${item.children.length} subagents`} aria-expanded={expanded} onPress={inTree ? undefined : () => onToggleChildren?.(item.ref)} {...stylex.props(styles.iconButton, styles.childrenToggle, variant === 'SR-2' && styles.childrenTop, variant === 'SR-1' && styles.childrenSingle)}><Icon name={expanded ? 'chevron-down' : 'chevron-right'} />{extraSignals.includes('X-subs') && <span data-row-signal="X-subs" data-line1-drop="subs">{item.children.length}</span>}</Button>}
      </div>
    </div>
    {!inTree && expanded && item.children.length > 0 && <div {...stylex.props(styles.children)}>{item.children.map(child => <SidebarAgentRow key={child.ref} item={child} variant={variant} layout={layout} glyph={glyph} extraSignals={extraSignals} discRefinement={discRefinement} query={query} {...clock} actions={actions} onOpen={onOpen} onMarkRead={onMarkRead} onToggleChildren={onToggleChildren} />)}</div>}
  </div>
})
const styles = stylex.create({
  wrapper: { minWidth: 0, width: '100%', containerType: 'inline-size' },
  rowWrapLeaf: { gridTemplateColumns: `${g.icon} minmax(0, 1fr) 0px var(--sidebar-metric-track, ${g.sidebarMetric}) max-content` },
  rowWrapParent: { gridTemplateColumns: `${g.icon} minmax(0, 1fr) ${g.icon} var(--sidebar-metric-track, ${g.sidebarMetric}) max-content` },
  rowWrap: { display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr) ${g.controlSm} var(--sidebar-metric-track, ${g.sidebarMetric}) max-content`, alignContent: 'center', alignItems: 'center', columnGap: s.xs2, rowGap: s.xs2, minWidth: 0, width: '100%', boxSizing: 'border-box', paddingInline: s.sm, borderRadius: r.sm, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fg, ':hover': { backgroundColor: surface.rowHover } },
  row: { display: 'grid', gridColumn: '2 / -1', gridRow: '1 / -1', gridTemplateColumns: 'subgrid', gridTemplateRows: 'subgrid', alignItems: 'center', minWidth: 0, width: '100%', height: '100%', padding: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, textAlign: 'start', cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: '-2px' } },
  'SR-1': { height: g.controlLg, gridTemplateRows: '1fr' }, 'SR-2': { height: g.sidebarRow, gridTemplateRows: `${g.controlSm} ${g.controlSm}`, rowGap: s.zero }, 'SR-3': { height: g.sidebarThree, gridTemplateRows: `${t.uiLeading} ${g.controlSm} ${t.metaLeading}` },
  active: { backgroundColor: surface.rowActive },
  glyph: { gridColumn: '1', gridRow: '1', display: 'flex', alignItems: 'center', justifyContent: 'center' },
  title: { gridColumn: '1 / 3', gridRow: '1', display: 'flex', alignItems: 'center', gap: s.xs2, whiteSpace: 'nowrap', fontWeight: t.weightMedium, minWidth: 0 },
  name: { flex: '1 1 0', minWidth: g.icon, overflow: 'hidden', textOverflow: 'ellipsis' },
  statusWord: { flexShrink: 0, color: ink.sidebarFgMuted, fontSize: t.denseSize, fontWeight: t.weightRegular },
  statusTime: { display: 'inline-flex', alignItems: 'center', justifyContent: 'end', gap: s.xs, gridColumn: '4', gridRow: '1', color: ink.sidebarFgMuted, fontSize: t.denseSize, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap', overflow: 'hidden' },
  timeCovered: { position: 'relative', visibility: 'hidden' },
  quickOpen: { position: 'absolute', insetInlineEnd: 0, insetBlock: 0, zIndex: 2, visibility: 'visible', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, boxSizing: 'border-box', padding: s.zero, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, cursor: 'pointer', ':hover': { backgroundColor: surface.rowActive }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  metric: { display: 'inline-flex', alignItems: 'baseline', gap: s.sm, flexShrink: 0 }, scope: { fontSize: t.microSize, color: ink.sidebarFgMuted },
  metadata: { gridColumn: '1 / -1', gridRow: '3', overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis', color: ink.sidebarFgMuted, fontSize: t.denseSize },
  actions: { display: 'contents' }, iconButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, minHeight: g.controlSm, borderWidth: 0, padding: s.zero, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.sidebarFgMuted, cursor: 'pointer', ':hover': { color: ink.fg, backgroundColor: surface.rowActive }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  menuButton: { gridColumn: '1', gridRow: '2', width: g.icon, opacity: 0 }, menuVisible: { opacity: 1 }, menuSingle: { gridColumn: '3', gridRow: '1' },
  childrenToggle: { gridColumn: '3', gridRow: '2', gap: s.xs2, width: g.controlSm }, childrenSingle: { gridRow: '1' }, childrenTop: { gridRow: '1', width: 'max-content', minWidth: g.icon },
  hidden: { display: 'none' },
  unread: { flexShrink: 0, color: accent.primary, fontSize: t.microSize, textAlign: 'center', fontVariantNumeric: 'tabular-nums' },
  needsYou: { flexShrink: 0, color: tone.attention, fontSize: t.denseSize, fontWeight: t.weightSemibold },
  hasChildren: { paddingInlineEnd: g.icon },
  trailingSignals: { gridColumn: '5', gridRow: '1', display: 'inline-flex', alignItems: 'center', justifyContent: 'end', gap: s.xs, width: `var(--sidebar-signal-track, ${g.status})`, marginInlineStart: s.xs, minWidth: g.status },
  hovercard: { backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, padding: s.lg, width: g.specimenAside, boxSizing: 'border-box', fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, maxWidth: 'calc(100vw - 32px)', maxHeight: g.commandMaxHeight, overflowY: 'auto', boxShadow: elevation.popover, zIndex: 30 },
  details: { display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 2fr)', columnGap: s.md, rowGap: s.sm, margin: s.zero, overflowWrap: 'anywhere' },
  card: { display: 'flex', flexDirection: 'column', gap: s.md, width: g.specimenAside, maxWidth: '100%', boxSizing: 'border-box', overflowWrap: 'anywhere' },
  cardTitle: { fontSize: t.uiSize, lineHeight: t.uiLeading },
  cardSummary: { display: 'flex', alignItems: 'center', gap: s.sm, color: ink.sidebarFgMuted },
  cardWork: { display: 'flex', alignItems: 'start', gap: s.sm, margin: s.zero, paddingBlock: s.sm, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  cardScope: { color: ink.sidebarFgMuted, fontSize: t.denseSize },
  cardLabel: { display: 'flex', alignItems: 'center', gap: s.sm, margin: s.zero, color: ink.sidebarFgMuted },
  cardChildren: { margin: s.zero, padding: s.zero, listStyleType: 'none', color: ink.sidebarFgMuted },
  detailLabel: { margin: s.zero, color: ink.sidebarFgMuted },
  detailValue: { margin: s.zero, minWidth: 0, color: ink.fg },
  popover: { backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, padding: s.sm, minWidth: g.menuMin, boxShadow: elevation.popover, fontFamily: t.fontSans, fontSize: t.metaSize, zIndex: 30 },
  menu: { outline: 'none' }, menuItem: { display: 'flex', alignItems: 'center', minHeight: g.menuRow, paddingInline: s.md, borderRadius: r.sm, cursor: 'pointer', outline: 'none', ':is([data-focused])': { backgroundColor: surface.rowActive }, ':is([data-focus-visible])': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  mark: { backgroundColor: surface.rowActive, color: ink.fg, fontWeight: t.weightSemibold }, children: { borderInlineStartWidth: g.hairline, borderInlineStartStyle: 'solid', borderInlineStartColor: border.border },
})

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Tabs, TabList, Tab, TabPanel, VisuallyHidden } from 'react-aria-components'
import { Composer } from '../assistant-ui/composition/Composer'
import { Transcript, type TranscriptTurn } from '../assistant-ui/composition/Transcript'
import { Icon } from '../assistant-ui/composition/Icons'
import { EmbraceRuntimeProvider } from '../assistant-ui/EmbraceRuntime'
import type { TextItem } from '../assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from '../assistant-ui/taste/work-log'
import { baselineTheme } from '../assistant-ui/neutral-theme'
import { lightTheme } from '../assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, statusVars as tone, borderVars as border, spaceVars as s, typeVars as t, radiusVars as r, geometryVars as g, accentVars as accent } from '../assistant-ui/composition-tokens.stylex'
import { switchingGeometry as geometry } from './switching-tokens.stylex'
import { alertWords, depthOf, unreportedExplanation, workspaces, type Agent, type Scenario } from './switching-fixtures'
import { SwitchingSidebar, type CountPlacement } from './SwitchingSidebar'

export const variants = ['S1', 'S2', 'S3'] as const
export type SwitcherVariant = typeof variants[number]
export const switcherNames: Record<SwitcherVariant, string> = { S1: 'Bottom switcher', S2: 'Top tabs', S3: 'Breadcrumb + thread drawer' }
/** `focusRequest` counts count-chip presses; `focusLane` names the lane whose switcher takes focus, so compare mode moves focus once. */
export interface SwitchingArgs { switcher: SwitcherVariant; placement: CountPlacement; sidebarWidth: 240 | 320; state: Scenario; compare: boolean; allLanes: boolean; scheme: 'dark' | 'light'; width: 1440 | 900; focusRequest: number; focusLane: string }
export const laneId = ({ switcher, scheme, width }: Pick<SwitchingArgs, 'switcher' | 'scheme' | 'width'>) => `${switcher}/${scheme}/${width}`

const now = Date.parse('2026-01-15T12:05:00Z')
const statusGlyph = { Running: 'spinner', Finished: 'check', Failed: 'x', 'Needs you': 'alert' } as const
/** Each child opens on its most telling thread; the root is the default everywhere else. */
const initialThread: Partial<Record<Scenario, string>> = { nested: 'great-grandchild', finished: 'child-1', failed: 'child-1', attention: 'child-1' }

function threadFor(agent: Agent, agents: readonly Agent[], sent: readonly string[]): TranscriptTurn[] {
  const parent = agents.find(candidate => candidate.id === agent.parent)
  const prompt: TextItem & { role: 'user' } = { _tag: 'Text', id: `${agent.id}/prompt`, role: 'user', text: agent.task, attachments: [], streaming: false, at: '2026-01-15T12:00:00Z', sender: parent === undefined ? { kind: 'human', label: 'Operator' } : { kind: 'agent', label: parent.name } }
  const text = { Failed: 'The navigation check stopped because the fixture could not load. The partial findings are preserved; the root agent can review the missing input.', 'Needs you': 'Should the switcher include finished agents by default? I have compared both paths and am waiting for your preference.', Finished: 'The navigation review is complete. Selection stays attached to the active thread, and returning to the root preserves the conversation.', Running: 'I am checking the navigation path. The root conversation stays available while this delegated thread continues independently.' }[agent.status]
  const answer: TextItem = { _tag: 'Text', id: `${agent.id}/answer`, role: 'assistant', text, attachments: [], streaming: false, at: '2026-01-15T12:01:00Z', sender: { kind: 'agent', label: agent.name } }
  const items: TextItem[] = [answer, ...sent.map((message, index): TextItem => ({ ...prompt, id: `${agent.id}/sent-${index}`, text: message, sender: { kind: 'human', label: 'Operator' } }))]
  return [{ id: agent.id, prompt, items, work: workLogTurnFromItems(items, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true }), senderCaptions: { [answer.id]: agent.name } }]
}

/**
 * Glyph, name, alert word and unread count are visible; the spoken separators, quiet statuses and "unread" are hidden text, so no control needs an aria-label.
 * Only the Failed and Needs you words are shown, in their status ink; running and finished speak through the glyph.
 */
function ThreadLabel({ agent, unread }: { agent: Agent; unread: number }) {
  const alert = agent.status === 'Failed' || agent.status === 'Needs you'
  return <>
    <span aria-hidden="true" {...stylex.props(styles.glyph, agent.status === 'Running' && styles.running, agent.status === 'Failed' && styles.failed, agent.status === 'Needs you' && styles.attention)}><Icon name={statusGlyph[agent.status]} /></span>
    <span {...stylex.props(styles.threadName)}>{agent.name}</span>
    {/* The leading spaces keep visible words apart in the text accessibility checkers read (flex layout drops them on screen), so label-in-name holds for "Delivery, Failed". */}
    {alert ? <><VisuallyHidden elementType="span">, </VisuallyHidden><span data-status-word {...stylex.props(styles.statusWord, agent.status === 'Failed' ? styles.failed : styles.attention)}>{' '}{agent.status}</span></> : <VisuallyHidden elementType="span">{`, ${agent.status.toLowerCase()}`}</VisuallyHidden>}
    {unread > 0 ? <><VisuallyHidden elementType="span">, </VisuallyHidden><span {...stylex.props(styles.unread)}>{' '}{unread}</span><VisuallyHidden elementType="span"> unread</VisuallyHidden></> : null}
  </>
}
/** Failed and needs-you threads the strip cannot show stay visible on the overflow control as glyph + count, and are spoken in words. */
function AlertCues({ agents }: { agents: readonly Agent[] }) {
  const words = alertWords(agents)
  if (words.length === 0) return null
  const failed = agents.filter(agent => agent.status === 'Failed').length
  const waiting = agents.filter(agent => agent.status === 'Needs you').length
  return <>
    {failed > 0 ? <span aria-hidden="true" data-alert-cue="failed" {...stylex.props(styles.cue, styles.failed)}><Icon name="x" size={12} />{failed}</span> : null}
    {waiting > 0 ? <span aria-hidden="true" data-alert-cue="needs-you" {...stylex.props(styles.cue, styles.attention)}><Icon name="alert" size={12} />{waiting}</span> : null}
    <VisuallyHidden elementType="span">{`, ${words.join(', ')}`}</VisuallyHidden>
  </>
}

export function SwitchingLane({ switcher: variant, state, scheme, width, placement, sidebarWidth, focusRequest, focusLane, landmarkContext, onOpenWorkspace }: SwitchingArgs & { landmarkContext?: string; onOpenWorkspace: (state: Scenario, focusSwitcher: boolean) => void }) {
  const fixture = workspaces[state]
  const agents = fixture.agents
  const focused = focusRequest > 0 && focusLane === laneId({ switcher: variant, scheme, width })
  const lane = React.useRef<HTMLElement>(null)
  const drawerId = React.useId()
  const [selected, setSelected] = React.useState(focused ? 'root' : initialThread[state] ?? 'root')
  const [drawer, setDrawer] = React.useState(false)
  const [messages, setMessages] = React.useState<Record<string, readonly string[]>>({})
  const [read, setRead] = React.useState<ReadonlySet<string>>(new Set())
  // Every count press, including one for the seat already open, returns to the root thread in the same render (React's adjust-state-during-render pattern).
  const [seenRequest, setSeenRequest] = React.useState(focusRequest)
  if (seenRequest !== focusRequest) {
    setSeenRequest(focusRequest)
    if (focused) setSelected('root')
  }
  /**
   * The count press's focus target — S1's open-thread chip, S2's selected tab, S3's drawer trigger — takes focus from the commit that attaches it.
   * The callback's identity changes per request, so React re-attaches it to the already mounted element for a repeat press, and React Aria's later tab mount consumes it on arrival.
   */
  const consumed = React.useRef(0)
  const focusTarget = React.useCallback((node: HTMLElement | null) => {
    if (!(node instanceof HTMLElement) || !focused || consumed.current === focusRequest) return
    consumed.current = focusRequest
    node.focus()
  }, [focused, focusRequest])
  const active = agents.find(agent => agent.id === selected) ?? agents[0]!
  const root = agents[0]!
  const turns = React.useMemo(() => threadFor(active, agents, messages[active.id] ?? []), [active, agents, messages])
  // The Composer sends through `onSend`; the runtime only renders the thread.
  const runtime = React.useMemo(() => ({ messages: turns.flatMap(turn => [turn.prompt!, ...turn.items]), isRunning: false, onNew: async () => {} }), [turns])
  const choose = (id: string) => { setSelected(id); setRead(current => new Set([...current, id])) }
  const cycle = (direction: number) => choose(agents[(agents.findIndex(agent => agent.id === active.id) + direction + agents.length) % agents.length]!.id)
  const unreadOf = (agent: Agent) => read.has(agent.id) ? 0 : agent.unread
  const context = (label: string) => landmarkContext === undefined ? label : `${label}, ${landmarkContext}`
  // Rail and tabs show what fits and keep the open thread visible; everything else is one press away in the drawer.
  const shown = agents.slice(0, width === 900 ? 3 : 6)
  if (!shown.includes(active)) shown.push(active)
  const path: Agent[] = [active]
  while (path[0]!.parent !== undefined) path.unshift(agents.find(agent => agent.id === path[0]!.parent)!)
  // Flex items are block-level, so name-from-content would read "Layout , running"; controls carry an aria-label in visible order (name, alert word, unread), then the quiet status.
  const threadLabel = (agent: Agent) => {
    const alert = agent.status === 'Failed' || agent.status === 'Needs you'
    return [agent.name, ...(alert ? [agent.status] : []), ...(unreadOf(agent) > 0 ? [`${unreadOf(agent)} unread`] : []), ...(alert ? [] : [agent.status.toLowerCase()])].join(', ')
  }
  // Named from content: the visible count, then the spoken alert words; the aria-hidden glyph cues stay out of the name.
  const drawerToggle = (text: string, beyond: readonly Agent[], target?: React.RefCallback<HTMLElement>) => <Button ref={target} data-thread-drawer-toggle aria-expanded={drawer} aria-controls={drawer ? drawerId : undefined} onPress={() => setDrawer(open => !open)} {...stylex.props(styles.chip, styles.quiet)}><Icon name="drawer" />{text}<AlertCues agents={beyond} /></Button>
  const beyondStrip = agents.filter(agent => !shown.includes(agent))
  const more = beyondStrip.length > 0 ? drawerToggle(`+${beyondStrip.length} more`, beyondStrip) : null
  // OMP and other unreported harnesses: say what is unknown instead of an empty child list or a spinner.
  const unreported = fixture.reporting === 'unreported' ? <p data-subagents-unreported {...stylex.props(styles.unreported)}>{unreportedExplanation(fixture)}</p> : null
  // st cannot address a child's own conversation yet (#2288): child threads are labelled samples with a real way back to the parent.
  const sampleNotice = active.parent === undefined ? null : <div role="note" data-sample-notice {...stylex.props(styles.sampleNotice)}>
    <span {...stylex.props(styles.tag)}>Sample</span>
    <span {...stylex.props(styles.sampleText)}>Child conversations are not addressable until #2288 ships.</span>
    <Button onPress={() => choose(root.id)} {...stylex.props(styles.chip)}>Open {root.name}</Button>
  </div>
  const conversation = <EmbraceRuntimeProvider options={runtime}>
    <div {...stylex.props(styles.conversation)}>
      {sampleNotice}
      <div {...stylex.props(styles.transcript)}><Transcript key={active.id} title={active.name} turns={turns} sync={{ _tag: 'Live', since: now - 5000 }} now={now} observedAt={now} landmarkContext={landmarkContext} /></div>
      <div {...stylex.props(styles.composerDock)}>
        {active.status === 'Needs you' ? <div role="status" data-needs-you {...stylex.props(styles.needsBar)}><span aria-hidden="true" {...stylex.props(styles.glyph, styles.attention)}><Icon name="alert" /></span><span {...stylex.props(styles.needsWord)}>Needs you</span><span {...stylex.props(styles.needsText)}>{active.name} is waiting on your preference. Reply in this thread.</span></div> : null}
        <Composer agent={active.name} draftKey={`switch-explore/${laneId({ switcher: variant, scheme, width })}/${state}/${active.id}`} onSend={message => setMessages(current => ({ ...current, [active.id]: [...(current[active.id] ?? []), message] }))} />
      </div>
    </div>
  </EmbraceRuntimeProvider>
  const threadDrawer = drawer ? <aside id={drawerId} aria-label={context('All agent threads')} data-thread-drawer data-switcher onKeyDown={event => { if (event.key === 'Escape') { setDrawer(false); lane.current?.querySelector<HTMLElement>('[data-thread-drawer-toggle]')?.focus() } }} {...stylex.props(styles.drawer)}>
    <header {...stylex.props(styles.drawerHeader)}><strong {...stylex.props(styles.drawerTitle)}>Threads</strong><span {...stylex.props(styles.muted)}>{agents.length}</span><span {...stylex.props(styles.fill)} /><Button aria-label="Close agent threads" onPress={() => setDrawer(false)} {...stylex.props(styles.iconButton)}><Icon name="x" /></Button></header>
    <ul {...stylex.props(styles.drawerList)}>{agents.map(agent => {
      const depth = depthOf(agent, agents)
      return <li key={agent.id} data-depth={depth} {...stylex.props(depth > 0 && styles.nested, depth === 2 && styles.depthTwo, depth === 3 && styles.depthThree)}><Button data-thread-item data-thread-current={agent.id === active.id ? true : undefined} aria-label={threadLabel(agent)} aria-current={agent.id === active.id ? 'true' : undefined} onPress={() => choose(agent.id)} {...stylex.props(styles.drawerItem, agent.id === active.id && styles.current)}><ThreadLabel agent={agent} unread={unreadOf(agent)} /></Button></li>
    })}</ul>
    {unreported === null ? null : <div {...stylex.props(styles.drawerNote)}>{unreported}</div>}
  </aside> : null
  const rail = <nav aria-label={context('Agent threads')} data-switcher data-rail {...stylex.props(styles.strip, styles.bottomStrip)}>
    {shown.map(agent => <Button key={agent.id} ref={agent.id === active.id ? focusTarget : undefined} data-thread-current={agent.id === active.id ? true : undefined} aria-label={threadLabel(agent)} aria-current={agent.id === active.id ? 'true' : undefined} onPress={() => choose(agent.id)} {...stylex.props(styles.chip, agent.id === active.id && styles.current)}><ThreadLabel agent={agent} unread={unreadOf(agent)} /></Button>)}
    {more}
    {unreported}
  </nav>
  const crumbs = <nav aria-label={context('Agent ancestry')} data-switcher {...stylex.props(styles.strip)}>
    <ol {...stylex.props(styles.crumbs)}>{path.map((agent, index) => <li key={agent.id} {...stylex.props(styles.crumb)}>
      {index > 0 ? <span aria-hidden="true" {...stylex.props(styles.separator)}><Icon name="chevron-right" /></span> : null}
      {agent === active ? <span aria-current="page" {...stylex.props(styles.crumbCurrent)}><ThreadLabel agent={agent} unread={0} /></span> : <Button onPress={() => choose(agent.id)} {...stylex.props(styles.crumbLink)}>{agent.name}</Button>}
    </li>)}</ol>
    {unreported}
    <span {...stylex.props(styles.fill)} />
    {drawerToggle(`All ${agents.length} ${agents.length === 1 ? 'thread' : 'threads'}`, agents.filter(agent => !path.includes(agent)), focusTarget)}
  </nav>
  const frame = <div {...stylex.props(styles.workspace)}>
    {variant === 'S3' ? crumbs : null}
    <div {...stylex.props(styles.body)}>{variant === 'S2' ? <TabPanel id={active.id} {...stylex.props(styles.panel)}>{conversation}</TabPanel> : conversation}{threadDrawer}</div>
    {variant === 'S1' ? rail : null}
  </div>
  return <section ref={lane} data-switching-lane data-variant={variant} data-state={state} data-scheme={scheme} data-width={width} data-active={active.id} aria-label={`${variant} ${switcherNames[variant]}, ${state}, ${scheme}, ${width}`} onKeyDown={event => { if (event.ctrlKey && (event.key === 'ArrowRight' || event.key === 'ArrowLeft')) { event.preventDefault(); cycle(event.key === 'ArrowRight' ? 1 : -1) } }} {...stylex.props(styles.lane)}>
    <header {...stylex.props(styles.caption)}><h2 {...stylex.props(styles.captionTitle)}>{variant} · {switcherNames[variant]}</h2><span>{state} · {scheme} · {width}</span>{fixture.sample ? <span data-sample-scenario {...stylex.props(styles.tag)}>Sample · child states need #2288</span> : null}<span><kbd {...stylex.props(styles.kbd)}>Ctrl</kbd> + <kbd {...stylex.props(styles.kbd)}>Left</kbd> / <kbd {...stylex.props(styles.kbd)}>Right</kbd> cycles every thread</span></header>
    <div data-lane-frame {...stylex.props(...baselineTheme, scheme === 'light' && lightTheme, styles.frame, width === 1440 ? styles.wide : styles.narrow, sidebarWidth === 240 ? styles.sidebar240 : styles.sidebar320)}>
      <div {...stylex.props(styles.sidebar)}><SwitchingSidebar placement={placement} selected={state} context={landmarkContext} onOpen={onOpenWorkspace} /></div>
      {variant === 'S2' ? <Tabs selectedKey={active.id} onSelectionChange={key => choose(String(key))} {...stylex.props(styles.tabsRoot)}>
        <div data-switcher {...stylex.props(styles.strip)}><TabList aria-label={context('Agent thread tabs')} {...stylex.props(styles.tabs)}>{shown.map(agent => <Tab id={agent.id} key={agent.id} ref={agent.id === active.id ? focusTarget : undefined} aria-label={threadLabel(agent)} data-thread-current={agent.id === active.id ? true : undefined} {...stylex.props(styles.tab, agent.id === active.id && styles.tabSelected)}><ThreadLabel agent={agent} unread={unreadOf(agent)} /></Tab>)}</TabList>{more}{unreported}</div>
        {frame}
      </Tabs> : frame}
    </div>
  </section>
}
const styles = stylex.create({
  lane: { display: 'flex', flexDirection: 'column', gap: s.md, flexShrink: 0 },
  caption: { display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: s.lg, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  captionTitle: { margin: s.zero, color: ink.fg, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightMedium },
  kbd: { fontFamily: t.fontSans, paddingInline: s.xs, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.keyboard },
  frame: { display: 'grid', height: geometry.height, overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md },
  wide: { width: geometry.wide }, narrow: { width: geometry.narrow },
  sidebar240: { gridTemplateColumns: geometry.columns240 }, sidebar320: { gridTemplateColumns: geometry.columns320 },
  sidebar: { minWidth: 0, overflowY: 'auto', padding: s.md, backgroundColor: surface.sidebar, borderRightWidth: g.hairline, borderRightStyle: 'solid', borderRightColor: border.border },
  tabsRoot: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0 },
  workspace: { display: 'flex', flexDirection: 'column', flex: 1, minWidth: 0, minHeight: 0 },
  body: { display: 'flex', flex: 1, minHeight: 0 },
  panel: { display: 'flex', flex: 1, minWidth: 0, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  conversation: { flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column' },
  transcript: { flex: 1, minHeight: 0, overflow: 'hidden' },
  composerDock: { display: 'flex', flexDirection: 'column', gap: s.md, padding: s.lg },
  needsBar: { display: 'flex', alignItems: 'center', gap: s.md, height: g.footer, boxSizing: 'border-box', paddingInline: s.lg, backgroundColor: surface.raised, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, fontSize: t.metaSize, lineHeight: t.metaLeading },
  needsWord: { flexShrink: 0, color: tone.attention, fontWeight: t.weightMedium },
  needsText: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: ink.fgSoft },
  strip: { display: 'flex', alignItems: 'center', gap: s.md, minWidth: 0, paddingBlock: s.md, paddingInline: s.lg, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  bottomStrip: { paddingTop: s.zero, borderBottomWidth: 0 },
  chip: { display: 'inline-flex', alignItems: 'center', gap: s.sm, flexShrink: 0, minHeight: g.controlLg, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  quiet: { color: ink.fgMuted },
  current: { backgroundColor: surface.rowActive, borderColor: border.borderStrong, color: ink.fg, ':hover': { backgroundColor: surface.rowActive } },
  tabs: { display: 'flex', gap: s.md, minWidth: 0, flex: 1, overflowX: 'auto' },
  tab: { display: 'inline-flex', alignItems: 'center', gap: s.sm, flexShrink: 0, minHeight: g.controlLg, paddingInline: s.md, borderBottomWidth: g.focusRing, borderBottomStyle: 'solid', borderBottomColor: surface.transparent, color: ink.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', outlineStyle: 'none', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  tabSelected: { borderBottomColor: ink.fg, color: ink.fg },
  crumbs: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0, margin: s.zero, padding: s.zero, listStyle: 'none', overflow: 'hidden' },
  crumb: { display: 'inline-flex', alignItems: 'center', gap: s.xs, minWidth: 0 },
  separator: { display: 'inline-flex', color: ink.fgMuted },
  crumbLink: { minHeight: g.controlLg, paddingInline: s.sm, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap', cursor: 'pointer', ':hover': { color: ink.fg, backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  crumbCurrent: { display: 'inline-flex', alignItems: 'center', gap: s.sm, paddingInline: s.sm, color: ink.fg, fontSize: t.metaSize, lineHeight: t.metaLeading },
  drawer: { display: 'flex', flexDirection: 'column', width: geometry.drawer, flexShrink: 0, overflowY: 'auto', backgroundColor: surface.raised, borderLeftWidth: g.hairline, borderLeftStyle: 'solid', borderLeftColor: border.border },
  drawerHeader: { display: 'flex', alignItems: 'center', gap: s.md, paddingBlock: s.md, paddingInline: s.lg, color: ink.fg, fontSize: t.metaSize, lineHeight: t.metaLeading },
  drawerTitle: { fontWeight: t.weightMedium },
  drawerList: { display: 'flex', flexDirection: 'column', gap: s.xs2, margin: s.zero, paddingInline: s.md, paddingBottom: s.md, listStyle: 'none' },
  // Nesting is 22px plus a 1px rail, the visual language's nesting step.
  nested: { marginInlineStart: geometry.indent, paddingInlineStart: s.xs, borderInlineStartWidth: g.hairline, borderInlineStartStyle: 'solid', borderInlineStartColor: border.borderStrong },
  depthTwo: { marginInlineStart: geometry.indentTwo }, depthThree: { marginInlineStart: geometry.indentThree },
  drawerItem: { display: 'flex', alignItems: 'center', gap: s.sm, width: '100%', minHeight: g.controlLg, paddingInline: s.md, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, textAlign: 'start', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  iconButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, height: g.controlSm, padding: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', ':hover': { color: ink.fg, backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  glyph: { display: 'inline-flex', flexShrink: 0, color: ink.fgMuted },
  running: { color: tone.runningFg }, failed: { color: tone.dangerFg }, attention: { color: tone.attention },
  threadName: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  unread: { flexShrink: 0, color: ink.fg, fontWeight: t.weightMedium, fontVariantNumeric: 'tabular-nums' },
  muted: { color: ink.fgMuted, fontVariantNumeric: 'tabular-nums' },
  fill: { flex: '1 1 0', minWidth: 0 },
  statusWord: { flexShrink: 0, whiteSpace: 'nowrap' },
  cue: { display: 'inline-flex', alignItems: 'center', gap: s.xs2, fontVariantNumeric: 'tabular-nums' },
  unreported: { margin: s.zero, minWidth: 0, color: ink.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  drawerNote: { paddingInline: s.lg, paddingBottom: s.md },
  // The 11/16 tag: 1px border, r6, no fill, muted ink.
  tag: { display: 'inline-flex', alignItems: 'center', flexShrink: 0, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, whiteSpace: 'nowrap' },
  sampleNotice: { display: 'flex', alignItems: 'center', gap: s.md, flexShrink: 0, paddingBlock: s.md, paddingInline: s.lg, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, fontSize: t.metaSize, lineHeight: t.metaLeading },
  sampleText: { flex: 1, minWidth: 0, color: ink.fgSoft },
})

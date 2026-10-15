import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { useDrag } from 'react-aria'
import { Button as AriaButton, Tooltip as AriaTooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { Icon, type IconName } from '../composition/Icons'
import { SidebarAgentRow } from '../sidebar/SidebarAgentRow'
import { ResourceCardV1, ResourceHoverV1, WholeResourceMeta } from '../resources/ResourceCardV1'
import { AgentHoverCard } from '../sidebar/SidebarAgentRow'
import { AGENT_DRAG_MIME, agentPaneKey } from '../workbench/agent-drag'
import type { DetailsResource, DetailsSection, DetailsState, DetailsValue } from './model'
import type { TraceSeries, TraceSubagent, TraceUnknownReason } from './trace-series'
import { DetailsInk } from './DetailsSkeleton'
import { accumulationParts, TraceCoverageNote } from './DetailsCharts'
import { DetailsAvailabilityState, DetailsUnavailable, MissingDetailsValue } from './DetailsUnavailable'
import { traceCoverageState } from './trace-series'

export type DetailsResourceForm = 'R1' | 'R2' | 'R3'
export type DetailsEmptyRule = 'E1' | 'E2' | 'E3'
export interface DetailsResourcesProps {
  readonly state: DetailsState
  readonly sections: readonly DetailsSection[]
  /** Breakdown facts are independent of the st resource snapshot. */
  readonly trace: TraceSeries
  readonly now: number
  readonly form?: DetailsResourceForm
  readonly empty?: DetailsEmptyRule
  readonly onOpen: (uri: string, split: boolean) => void
  /** Synced layout intent (q45): section names the parent keeps hidden. */
  readonly hiddenSections?: readonly string[]
  readonly styles?: stylex.StyleXStyles
}
export const DetailsLabelContext=React.createContext('')

/** One pane-level column; collapse, hover and skeletons stay local, never synced. */
export function DetailsResources({ state, sections: suppliedSections, trace, now, form = 'R1', empty = 'E1', onOpen, hiddenSections = [], styles: callerStyles }: DetailsResourcesProps) {
  const labelPrefix=React.useContext(DetailsLabelContext)
  // Pending geometry derives only from the host's supplied observations.
  const observedSubagents = suppliedSections.find(section => section.title === 'Subagents')
  // Known [] is an observed empty st resource list, distinct from an unreported one; it is evidence once the agent has started.
  const hasObservedSubagents = observedSubagents?.items._tag === 'Known'
  const hasObservedSubagentRows = observedSubagents?.items._tag === 'Known' && observedSubagents.items.value.length > 0
  const hasNoTracedSubagents = knownZeroTracedSubagents(trace)
  const [collapsed, setCollapsed] = React.useState<Readonly<Record<string, boolean>>>({})
  const hidden = React.useMemo(() => new Set(hiddenSections), [hiddenSections])
  const toggle = React.useCallback((title: string) => setCollapsed(current => ({ ...current, [title]: !current[title] })), [])
  const open = React.useCallback((uri: string, split: boolean) => onOpen(agentPaneKey(uri), split), [onOpen])
  const candidates = state === 'error' ? [] : suppliedSections
    .map(section => section.title === 'Subagents' ? { ...section, ...traceSubagentSection(trace) } : section.title === 'History' ? { ...section, ...traceHistorySection(trace) } : section)
    .filter(section => !hidden.has(section.title))
  const hasSubagentEvidence = hasObservedSubagents || hasNoTracedSubagents || trace.subagentCount._tag === 'Known'
  const unreported = empty === 'E3' && state !== 'new'
    ? candidates.filter(section => !section.pending && section.items._tag === 'Unknown' && !(section.title === 'Subagents' && hasSubagentEvidence))
    : []
  // Pane-level loading keeps the settled footprint: the Not supplied line is reserved, unpainted and inert, so recovery never grows the slot.
  const reserveUnreported = state === 'loading'
  const sections = candidates
    // First reads stay pending, never masquerading as settled unavailable data.
    .filter(section => section.pending === true || state !== 'new' || empty !== 'E3' || (section.items._tag === 'Known' && section.items.value.length > 0) || (section.title === 'Subagents' && hasObservedSubagentRows))
    .filter(section => section.pending === true || visible(section.items, empty) || (section.title === 'Subagents' && hasSubagentEvidence))
  return <div data-details-state={state} data-details-form={form} data-details-empty={empty} {...stylex.props(styles.root, callerStyles)}>
    {sections.map(section => {
      const isCollapsed = collapsed[section.title] === true
      // The Subagents badge is the reported traced total, never the retained list length; an unknown total has no badge.
      const count = section.title === 'Subagents' ? trace.subagentCount._tag === 'Known' ? trace.subagentCount.value : undefined
        : section.items._tag === 'Known' && section.items.value.length > 0 ? section.items.value.length : undefined
      const pending = state === 'loading' || section.pending === true
      if (!pending && section.items._tag === 'Unknown' && !(section.title === 'Subagents' && hasSubagentEvidence)) return <section key={section.title} aria-label={`${labelPrefix}${section.title}`} data-details-section-state="unavailable" {...stylex.props(styles.section)}>
        <div {...stylex.props(styles.header)}><span {...stylex.props(styles.chevronSpace)} /><span data-details-heading {...stylex.props(styles.metaLine)}>{section.title}</span><span {...stylex.props(styles.metaLine)}>·</span><DetailsAvailabilityState subject={section.title} reason={unavailableReason(section, trace, empty)} /></div>
      </section>
      return <section key={section.title} aria-label={`${labelPrefix}${section.title}`} aria-busy={pending || undefined} {...stylex.props(styles.section)}>
        <div {...stylex.props(styles.header)}>
          <AriaButton aria-expanded={!isCollapsed} onPress={() => toggle(section.title)} {...stylex.props(styles.headerButton)}>
            <Icon name={isCollapsed?'chevron-right':'chevron-down'}/>
            <span data-details-heading {...stylex.props(styles.headerTitle)}>{section.title}</span>
            {count !== undefined && (pending
              ? <span aria-hidden="true" data-details-skeleton-count {...stylex.props(styles.count)}><DetailsInk ch={String(count).length} /></span>
              : <span data-details-section-count {...stylex.props(styles.count)}>{count}</span>)}
          </AriaButton>
        </div>
        {!isCollapsed && <div {...stylex.props(styles.body)}>
          <SectionContent section={section} observedSubagents={observedSubagents} trace={trace} now={now} form={form} empty={empty} open={open} pending={pending} />
        </div>}
      </section>
    })}
    {unreported.length > 0 && <p data-details-unreported-sections={reserveUnreported ? undefined : true} data-details-unreported-reserved={reserveUnreported || undefined} aria-hidden={reserveUnreported || undefined} inert={reserveUnreported} {...stylex.props(styles.metaLine, styles.allHidden, reserveUnreported && styles.skeletonHidden)}>Not supplied: {unreported.map((section, index) => <React.Fragment key={section.title}>{index > 0 && ', '}<TooltipTrigger delay={0}><AriaButton data-details-unreported-section={reserveUnreported ? undefined : section.title} {...stylex.props(styles.unreportedTrigger)}>{section.title}</AriaButton><AriaTooltip {...stylex.props(styles.tooltip)}>{unavailableReason(section, trace, empty)}</AriaTooltip></TooltipTrigger></React.Fragment>)}.</p>}
    {sections.length === 0 && unreported.length === 0 && state !== 'error' && !(state === 'new' && empty === 'E3') && <p {...stylex.props(styles.metaLine, styles.allHidden)}>Every section is empty or hidden.</p>}
  </div>
}

/** E1 and E3 hide empty and Unknown bodies; E3 names settled unavailable categories once. E2 shows everything. */
function visible(items: DetailsValue<readonly DetailsResource[]>, rule: DetailsEmptyRule): boolean {
  if (rule === 'E2') return true
  if (items._tag === 'Unknown') return false
  return items.value.length > 0
}

/** An explicitly reported zero is data, not an empty-list inference. */
const knownZeroTracedSubagents = (trace: TraceSeries): boolean => trace.subagentCount._tag === 'Known'
  && trace.subagentCount.value === 0 && trace.subagents._tag === 'Known' && trace.subagents.value.length === 0

/** Collection provenance stays in the missing-value tooltip. */
const humanSubagentReasons: Readonly<Record<TraceUnknownReason, string>> = {
  'no-provider': 'This source doesn’t report subagent breakdowns.',
  'not-covered': 'Subagent breakdowns aren’t tracked for this session.',
  'not-attributed': 'Subagent breakdowns aren’t attributed to this session.',
  'out-of-retention': 'Subagent breakdowns are no longer retained.',
  loading: 'Subagent breakdowns are still loading.',
  failed: 'Subagent breakdowns couldn’t be loaded.',
}
const humanSectionReasons: Readonly<Partial<Record<DetailsSection['title'], string>>> = {
  Reviews: 'This source doesn’t report reviews.',
  'Declared bindings': 'This source doesn’t report declared bindings.',
  History: 'This source doesn’t report session history.',
}
function unavailableReason(section: DetailsSection, trace: TraceSeries, empty: DetailsEmptyRule): string {
  if (section.items._tag === 'Unknown' && section.unavailableReason !== undefined) return section.unavailableReason
  if (section.title === 'Subagents' && trace.subagents._tag === 'Unknown' && trace.subagents.detail !== undefined) return trace.subagents.detail
  if (section.title === 'History' && trace.history._tag === 'Unknown' && trace.history.detail !== undefined) return trace.history.detail
  const subagentReason = trace.subagents._tag === 'Unknown' ? trace.subagents.reason : 'no-provider'
  if (empty !== 'E3') return section.title === 'Subagents' ? humanSubagentReasons[subagentReason] : `This source doesn’t report ${section.title.toLowerCase()} yet.`
  return section.title === 'Subagents' ? humanSubagentReasons[subagentReason] : humanSectionReasons[section.title] ?? `This source doesn’t report ${section.title.toLowerCase()}.`
}

/** Name only missing row metrics; unrelated unreported facts need no section note. */
function missingLineCountsNote(section: DetailsSection): string | undefined {
  if (section.items._tag !== 'Known') return undefined
  let added = false, removed = false
  for (const resource of section.items.value) {
    added ||= resource.file?.added._tag === 'Unknown'
    removed ||= resource.file?.removed._tag === 'Unknown'
  }
  if (!added && !removed) return undefined
  return `${added && removed ? 'Added- and removed-line' : added ? 'Added-line' : 'Removed-line'} counts aren't reported for some files.`
}

/** No native-session-to-st-agent identity is supplied; traced sessions stay neutral. */
export function traceSubagentSection(trace: TraceSeries): DetailsSection {
  if (trace.subagents._tag !== 'Known') return { title: 'Subagents', items: { _tag: 'Unknown' } }
  return {
    title: 'Subagents',
    items: { _tag: 'Known', value: trace.subagents.value.map(subagent => {
      // Reported role, known model and full native session identity are visible before Open; a generic role stays generic.
      const label = [subagent.label._tag === 'Known' ? subagent.label.value : 'role unreported', subagent.model._tag === 'Known' ? subagent.model.value : undefined, subagent.sessionId].filter(value => value !== undefined).join(' · ')
      const detail = traceSubagentDetail(subagent)
      return {
        uri: `session:${subagent.sessionId}`,
        glyph: '◇',
        card: { kind: 'file', label, detail: `Traced session · ${detail}`, fileCount: { _tag: 'Unknown' }, files: [] },
      }
    }) },
  }
}
/** Missing descendant facts are omitted, never fabricated parentage or timing; the model is part of the title. */
const traceSubagentDetail = (subagent: TraceSubagent): string => [
  `$${subagent.usd.toFixed(2)}`,
  `${subagent.requests} requests`,
  subagent.parentSessionId._tag === 'Known' ? `Parent · ${subagent.parentSessionId.value}` : undefined,
  subagent.firstMs._tag === 'Known' ? `First observed ${historyTimestamp.format(subagent.firstMs.value)} UTC` : undefined,
  subagent.lastMs._tag === 'Known' ? `Last observed ${historyTimestamp.format(subagent.lastMs.value)} UTC` : undefined,
].filter(value => value !== undefined).join(' · ')

const historyTimestamp=new Intl.DateTimeFormat('en-GB',{dateStyle:'medium',timeStyle:'short',timeZone:'UTC'})
/** Incarnations retain their provider identity and observed lifetime facts. */
export function traceHistorySection(trace: TraceSeries): DetailsSection {
  if(trace.history._tag!=='Known')return {title:'History',items:{_tag:'Unknown'}}
  return {title:'History',items:{_tag:'Known',value:trace.history.value.map(incarnation=>({
    uri:`session:${incarnation.nativeSessionId}`,
    glyph:'◷',
    card:{
      kind:'file',
      label:`Incarnation · ${incarnation.nativeSessionId}`,
      detail:`Started ${historyTimestamp.format(incarnation.startedMs)} UTC${incarnation.endedMs===null?'':` · Ended ${historyTimestamp.format(incarnation.endedMs)} UTC`} · $${incarnation.usd.toFixed(2)} · ${incarnation.compactions} ${incarnation.compactions === 1 ? 'compaction' : 'compactions'}`,
      fileCount:{_tag:'Unknown'},
      files:[],
    },
  }))}}
}

/** The loading shadow and populated body share markup, wrapping and intrinsic metadata fit. */
function SectionContent({ section, observedSubagents, trace, now, form, empty, open, pending }: {
  section: DetailsSection; observedSubagents: DetailsSection | undefined; trace: TraceSeries; now: number;
  form: DetailsResourceForm; empty: DetailsEmptyRule; open: (uri: string, split: boolean) => void; pending: boolean
}) {
  const hasObserved = observedSubagents?.items._tag === 'Known'
  // A first-read Subagents section reserves its breakdown; pane-level loading keeps the settled footprint.
  const showBreakdown = section.pending === true || section.title !== 'Subagents' || visible(section.items, empty) || trace.subagentCount._tag === 'Known'
  const subagentWindow=section.title==='Subagents'?accumulationParts(trace.subagentsWindow):undefined
  const collectionReason = showBreakdown && section.items._tag === 'Unknown' ? unavailableReason(section, trace, empty) : undefined
  const observedReason = section.title === 'Subagents' && observedSubagents?.items._tag === 'Unknown'
    ? observedSubagents.unavailableReason ?? 'This source doesn’t report st subagent resources.' : undefined
  const missingStatus = section.title === 'Subagents' && observedSubagents?.items._tag === 'Known'
    && observedSubagents.items.value.some(resource => resource.agent?.status === 'unobserved')
  const partialNote = missingStatus ? "Status isn't reported for st resources." : missingLineCountsNote(section)
  const windowReason = section.title === 'Subagents' && showBreakdown && subagentWindow === undefined
    ? trace.subagentsWindow._tag === 'Unknown' ? trace.subagentsWindow.detail ?? `Traced-session accumulation window: ${trace.subagentsWindow.reason}.` : 'Traced-session accumulation window is unreported.'
    : undefined
  const countReason = section.title === 'Subagents' && showBreakdown && trace.subagents._tag === 'Known' && trace.subagentCount._tag === 'Unknown'
    ? trace.subagentCount.detail ?? `Traced-session total count: ${trace.subagentCount.reason}.` : undefined
  const coverage = traceCoverageState(trace.coverage)
  const coverageReason = section.title === 'Subagents'
    ? coverage._tag === 'Unavailable' ? `Attribution coverage: ${coverage.reason}.`
      : coverage._tag === 'PartialUnreported' ? 'The attributed fraction is unreported.' : undefined
    : undefined
  const gaps = [
    { subject: 'Traced sessions', reason: collectionReason, partial: false },
    { subject: 'Observed st resources', reason: observedReason, partial: false },
    { subject: missingStatus ? 'Status' : 'Line counts', reason: partialNote, partial: missingStatus
      ? observedSubagents?.items._tag === 'Known' && observedSubagents.items.value.some(resource => resource.agent !== undefined && resource.agent.status !== 'unobserved')
      : section.items._tag === 'Known' && section.items.value.some(resource => resource.file?.added._tag === 'Known' || resource.file?.removed._tag === 'Known') },
    { subject: 'Traced-session window', reason: windowReason, partial: false },
    { subject: 'Traced-session total', reason: countReason, partial: false },
    { subject: 'Attribution', reason: coverageReason, partial: coverage._tag === 'PartialUnreported' },
  ].filter((gap): gap is { subject: string; reason: string; partial: boolean } => gap.reason !== undefined)
  return <div aria-hidden={pending || undefined} inert={pending} data-details-skeleton={pending ? section.title : undefined} {...stylex.props(styles.contentShell)}>
    <div {...stylex.props(styles.content, pending && styles.skeletonHidden)}>
      {section.title === 'Subagents' && hasObserved && observedSubagents !== undefined && observedSubagents.items._tag === 'Known' && (observedSubagents.items.value.length === 0
        ? <SkeletonLine pending={pending}><p data-details-observed-subagent-count {...stylex.props(styles.metaLine)}>Observed st resources · 0</p></SkeletonLine>
        : <SectionItems section={observedSubagents} now={now} form={form} open={open} pending={pending} />)}
      {showBreakdown && <>
        {section.title === 'Subagents' && <SkeletonLine pending={pending}><p data-details-subagent-window {...stylex.props(styles.metaLine)}>Traced sessions · {subagentWindow === undefined ? <MissingDetailsValue label="Traced-session accumulation window" reason={windowReason ?? 'Traced-session accumulation window is unreported.'} /> : subagentWindow.join('')}</p></SkeletonLine>}
        {section.title === 'Subagents' && trace.subagents._tag === 'Known' && <SkeletonLine pending={pending}><p data-details-subagent-count {...stylex.props(styles.metaLine)}>Showing {trace.subagents.value.length} of {trace.subagentCount._tag === 'Known' ? trace.subagentCount.value : <MissingDetailsValue label="Total traced-session count" reason={countReason ?? 'Traced-session total count is unreported.'} />} traced sessions</p></SkeletonLine>}
        {section.title === 'Subagents' && (coverage._tag === 'Partial' || coverage._tag === 'NotCovered' || coverage._tag === 'PartialUnreported') && <SkeletonLine pending={pending}><TraceCoverageNote trace={trace} /></SkeletonLine>}
        {section.items._tag === 'Unknown'
          ? null
          : section.items.value.length === 0
            ? <SkeletonLine pending={pending}><p {...stylex.props(styles.metaLine)}>None</p></SkeletonLine>
            : <SectionItems section={section} now={now} form={form} open={open} pending={pending} />}
      </>}
      {gaps.length > 0 && <SkeletonLine pending={pending}><DetailsUnavailable subjects={gaps.map(gap => gap.subject)} state={gaps.every(gap => gap.partial) ? 'Partial' : 'Unavailable'} reason={gaps.map(gap => gap.reason).join(' ')} /></SkeletonLine>}
    </div>
  </div>
}

/**
 * Pending rows keep their populated markup for exact geometry and paint one
 * row-shaped skeleton over it, like Context and Spend ink, instead of a section
 * block. The wrapper re-shows only the skeleton inside the hidden content.
 */
function SkeletonRow({ pending, label, meta, children }: { pending: boolean; label: string; meta?: string; children: React.ReactNode }) {
  if (!pending) return children
  return <div data-details-skeleton-row {...stylex.props(styles.skeletonRow)}>
    <div {...stylex.props(styles.skeletonHidden)}>{children}</div>
    <div {...stylex.props(styles.skeletonInk)}>
      <DetailsInk ch={2} />
      <span {...stylex.props(styles.skeletonLabel)}><DetailsInk ch={Math.min(label.length, 32)} /></span>
      {meta !== undefined && meta !== '' && <DetailsInk ch={Math.min(meta.length, 16)} />}
    </div>
  </div>
}

function SkeletonLine({ pending, children }: { pending: boolean; children: React.ReactNode }) {
  if (!pending) return children
  return <div data-details-skeleton-row {...stylex.props(styles.skeletonRow)}>
    <div {...stylex.props(styles.skeletonHidden)}>{children}</div>
    <div {...stylex.props(styles.skeletonInk)}><DetailsInk ch={18} /></div>
  </div>
}

function wrapsIdentity(resource: DetailsResource): boolean {
  return resource.uri.startsWith('branch:') || resource.uri.startsWith('pr:') || resource.uri.startsWith('mission:')
}

function SectionItems({ section, now, form, open, pending }: { section: DetailsSection; now: number; form: DetailsResourceForm; open: (uri: string, split: boolean) => void; pending: boolean }) {
  const [expanded,setExpanded]=React.useState(false)
  const items = section.items._tag === 'Known' ? section.items.value : []
  const capped = form === 'R3' && items.length > 3
  const shown = capped && !expanded ? items.slice(0, 3) : items
  // An absent agent status must not fall back to the card's projected status text.
  const row = (resource: DetailsResource, child: React.ReactNode) => <SkeletonRow key={resource.uri} pending={pending} label={resource.card.label} meta={resource.agent === undefined ? resource.card.detail : resource.agent.status === 'unobserved' ? undefined : resource.agent.statusLabel}>{child}</SkeletonRow>
  return <>
    {form === 'R2' && <div role="list" aria-label={section.title} {...stylex.props(styles.dense)}>{shown.map(resource => row(resource, <DenseRow resource={resource} now={now} open={open} />))}</div>}
    {form !== 'R2' && shown.map(resource => row(resource, <ResourceEntry resource={resource} now={now} open={open} />))}
    {capped && <SkeletonLine pending={pending}><AriaButton data-details-quiet-action onPress={()=>setExpanded(value=>!value)} {...stylex.props(styles.splitButton)}>{expanded?'Show fewer':`+${items.length - shown.length} more`}</AriaButton></SkeletonLine>}
  </>
}

/** Terminal previews are observed lines; diff and file previews reuse the canonical hover. */
function HoverContent({ resource, now }: { resource: DetailsResource; now: number }) {
  if (resource.agent !== undefined) return <AgentHoverCard row={resource.agent} reportedStatusOnly now={now} />
  return <div {...stylex.props(styles.hover)}>
    <strong {...stylex.props(styles.hoverTitle)}>{resource.card.label}</strong>
    {resource.terminal !== undefined
      ? <pre {...stylex.props(styles.terminalPreview)}>{resource.terminal.join('\n')}</pre>
      : <ResourceHoverV1 resource={resource.card} file={resource.file} />}
  </div>
}

function ResourceEntry({ resource, now, open }: { resource: DetailsResource; now: number; open: (uri: string, split: boolean) => void }) {
  const paneKey = agentPaneKey(resource.uri)
  const getItems = React.useCallback(() => [{ [AGENT_DRAG_MIME]: paneKey, 'text/plain': paneKey }], [paneKey])
  const { dragProps } = useDrag({ getItems, getAllowedDropOperations: () => ['copy'] })
  const splitPress=React.useRef(false)
  const capture=(event:React.MouseEvent|React.PointerEvent|React.KeyboardEvent)=>{splitPress.current=event.metaKey||event.ctrlKey}
  if (resource.agent !== undefined) {
    return <div {...dragProps} onPointerDownCapture={capture} onKeyDownCapture={capture} onClickCapture={capture} {...stylex.props(styles.agentEntry)}>
      <div {...stylex.props(styles.agentRow)}><SidebarAgentRow item={resource.agent} variant="SR-2" reportedStatusOnly now={now} onOpen={row => open(agentPaneKey(row.ref), splitPress.current)} /></div>
      <AriaButton data-details-quiet-action aria-label={`Open in split: ${resource.card.label}`} onPress={() => open(paneKey, true)} {...stylex.props(styles.splitButton, styles.inlineSplit)}>Open in split</AriaButton>
    </div>
  }
  // Traced sessions and incarnations carry their reported facts only in the detail, so the card paints it.
  return <div {...dragProps} onPointerDownCapture={capture} onKeyDownCapture={capture} onClickCapture={capture} {...stylex.props(styles.entry)}>
    <ResourceCardV1 resource={resource.card} variant="RC-1" titlePriority titleLines={wrapsIdentity(resource) || resource.uri.startsWith('session:') ? 'wrap' : 1} icon={resourceIcon(resource)} detailLine={resource.uri.startsWith('session:')} onOpen={() => open(paneKey, splitPress.current)} />
  </div>
}

/** Resource concepts use the same stroke kit as section controls, never text glyphs. */
function resourceIcon(resource: DetailsResource): IconName {
  switch (resource.uri.split(':', 1)[0]) {
    case 'worktree':
    case 'repository': return 'folder'
    case 'branch':
    case 'pr': return 'swap'
    case 'ci-run': return resource.card.detail.endsWith('running') ? 'clock' : 'check'
    case 'review': return 'check'
    case 'diff': return 'pencil'
    case 'terminal': return 'chevron-right'
    case 'agent':
    case 'mission': return 'message'
    case 'attention': return 'alert'
    case 'session': return resource.card.label.startsWith('Incarnation · ') ? 'clock' : 'message'
    default: return resource.card.kind === 'diff' ? 'pencil' : 'attach'
  }
}

/** Dense rows retain aligned metadata while branch and PR identities get two lines; painted text names the row and its facts describe it. */
function DenseRow({ resource, now, open }: { resource: DetailsResource; now: number; open: (uri: string, split: boolean) => void }) {
  const paneKey = agentPaneKey(resource.uri)
  const getItems=React.useCallback(()=>[{[AGENT_DRAG_MIME]:paneKey,'text/plain':paneKey}],[paneKey])
  const {dragProps}=useDrag({getItems,getAllowedDropOperations:()=>['copy']})
  const meta = resource.terminal !== undefined ? 'terminal'
    : resource.agent !== undefined ? resource.agent.status === 'unobserved' ? undefined : resource.agent.statusLabel
    : resource.file !== undefined
      ? [resource.file.added._tag === 'Known' && resource.file.added.value !== 0 ? `+${resource.file.added.value}` : '', resource.file.removed._tag === 'Known' && resource.file.removed.value !== 0 ? `−${resource.file.removed.value}` : ''].filter(Boolean).join(' ')
    : resource.card.detail
  const isSubagent = resource.agent !== undefined || resource.uri.startsWith('session:')
  // An absent agent status must not fall back to the card's projected status text; whole meta may be hidden when it does not fit.
  const facts = [...new Set([meta, resource.agent === undefined ? resource.card.detail : undefined, resource.file !== undefined && resource.file.path !== resource.card.label ? resource.file.path : undefined])].filter((fact): fact is string => fact !== undefined && fact !== '')
  const descriptionId = React.useId()
  return <div data-resource-card role="listitem" {...dragProps} {...stylex.props(styles.denseEntry)}><TooltipTrigger delay={150}>
    <AriaButton data-details-resource-row aria-describedby={facts.length > 0 ? descriptionId : undefined} onPress={event => open(paneKey, event.metaKey || event.ctrlKey)} {...stylex.props(styles.denseRow)}>
      <span data-details-resource-icon={resourceIcon(resource)} {...stylex.props(styles.resourceIcon)}><Icon name={resourceIcon(resource)} /></span>
      <span data-details-resource-title data-resource-title-lines={wrapsIdentity(resource) ? 'wrap' : 1} {...stylex.props(styles.denseLabel, wrapsIdentity(resource) && styles.wrappedLabel)}>{resource.card.label}</span>{' '}
      <WholeResourceMeta numeric={resource.file !== undefined} styles={styles.denseMeta}>{meta}</WholeResourceMeta>
    </AriaButton>
    <AriaTooltip placement="left" {...stylex.props(styles.tooltip)}><HoverContent resource={resource} now={now} /></AriaTooltip>
  </TooltipTrigger>{facts.length > 0 && <VisuallyHidden elementType="span"><span id={descriptionId} data-details-resource-facts>{facts.join(' · ')}</span></VisuallyHidden>}{isSubagent && <AriaButton aria-label={`Open ${resource.card.label} in split`} onPress={() => open(paneKey, true)} {...stylex.props(styles.denseSplit)}><Icon name="panel" /></AriaButton>}</div>
}

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, fontFamily: t.fontSans, color: ink.fg },
  section: { display: 'flex', flexDirection: 'column', minWidth: 0 },
  header: { minHeight: g.toolRow, display: 'flex', alignItems: 'center', gap: s.xs, flexShrink: 0, paddingInline: s.md },
  headerButton: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0, flexGrow: 1, height: g.toolRow, padding: s.zero, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, textAlign: 'left', cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  headerTitle: { color: ink.fg, fontWeight: t.weightSemibold, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' },
  count: { fontFamily: t.fontMono, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted, flexShrink: 0 },
  body: { display: 'flex', flexDirection: 'column', gap: s.sm, paddingInlineStart: s.xxl, paddingInlineEnd: s.md, paddingBottom: s.md, minWidth: 0 },
  metaLine: { margin: s.zero, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted },
  allHidden: { padding: s.md },
  chevronSpace: { width: g.icon, flexShrink: 0 },
  unreportedTrigger: { display: 'inline', padding: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'help', ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  entry: { display: 'flex', flexDirection: 'column', minWidth: 0 },
  agentEntry: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0 },
  agentRow: { flex: 1, minWidth: 0 },
  inlineSplit: { flexShrink: 0, whiteSpace: 'nowrap' },
  splitButton: { height: g.controlSm, paddingInline: s.sm, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, borderRadius: r.sm, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': { outline: `2px solid ${accent.primary}` } },
  dense: { display: 'flex', flexDirection: 'column', minWidth: 0 },
  denseEntry: { position: 'relative', display: 'flex', alignItems: 'center', minWidth: 0 },
  denseSplit: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0, width: g.toolRow, height: g.toolRow, padding: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  denseRow: { display: 'flex', alignItems: 'center', gap: s.md, width: '100%', minWidth: 0, flex: 1, boxSizing: 'border-box', minHeight: g.toolRow, paddingInline: s.sm, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, textAlign: 'left', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg }, ':focus-visible': { outline: `2px solid ${accent.primary}`, outlineOffset: -2 } },
  resourceIcon: { display: 'inline-flex', flexShrink: 0, color: ink.fgMuted },
  denseLabel: { flexBasis: 0, flexGrow: 1, flexShrink: 0, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' },
  // Identity labels wrap in full (no clamp) with balanced lines: a long branch never leaves one orphaned word beside its badge.
  wrappedLabel: { whiteSpace: 'normal', overflow: 'visible', overflowWrap: 'anywhere', textWrap: 'balance' },
  denseMeta: { marginInlineStart: 'auto', color: ink.fgMuted, fontSize: t.microSize },
  contentShell: { display: 'flex', flexDirection: 'column', minWidth: 0 },
  content: { display: 'flex', flexDirection: 'column', gap: s.sm, minWidth: 0 },
  skeletonHidden: { visibility: 'hidden' },
  skeletonRow: { position: 'relative', minWidth: 0 },
  skeletonInk: { visibility: 'visible', position: 'absolute', inset: s.zero, display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.sm, overflow: 'hidden', pointerEvents: 'none' },
  skeletonLabel: { display: 'flex', flexGrow: 1, minWidth: 0, overflow: 'hidden' },
  hover: { maxWidth: 'min(360px, calc(100vw - 24px))', display: 'flex', flexDirection: 'column', gap: s.xs },
  hoverTitle: { fontSize: t.metaSize, fontWeight: t.weightMedium },
  tooltip: { padding: s.md, backgroundColor: surface.raised, color: ink.fg, borderRadius: r.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, fontSize: t.metaSize, lineHeight: t.metaLeading, zIndex: 10 },
  terminalPreview: { margin: s.zero, padding: s.sm, backgroundColor: surface.codeBg, borderRadius: r.sm, fontFamily: t.fontMono, fontSize: t.metaSize, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
})

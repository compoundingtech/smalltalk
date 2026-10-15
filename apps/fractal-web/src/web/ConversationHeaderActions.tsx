import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { RegistryContext, useAtom, useAtomValue } from '@effect/atom-react'
import * as Atom from 'effect/reactivity/Atom'
import { DateTime } from 'effect'
// Deep import: the assistant-ui barrel would pull the transcript graph into the eager shell.
import { Icon as CompositionIcon } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Icons.tsx'
import { SidePanel } from '../../../../packages/fractal-ui/src/assistant-ui/composition/SidePanel.tsx'
import { colorVars as c, typeVars as t, spaceVars as s } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { HumanTime } from '@smalltalk/fractal-ui/human-time'
import { useDataSource } from '../data/react.tsx'
import { unavailable, waiting, type DataSource, type Feed } from '../data/source.ts'
import { LiveAgentTodos } from '../conversation/todos/AgentTodos.tsx'
import { resourceState, resourceTitle, type ResourcePage } from '../resources/agent/model.ts'
import { systemEventsPreference } from './conversationPreferences.ts'
import type { OpenOperation, UxTelemetry } from '../telemetry/ux.ts'

const unsupportedResources = Atom.make<Feed<ResourcePage>>(unavailable({ reason: 'unsupported', detail: '' }))
const unopenedResources = Atom.make<Feed<ResourcePage>>(waiting)

interface ResourceAtoms {
  readonly resources: (ref: string) => Atom.Atom<Feed<ResourcePage>>
  readonly queue: DataSource['subjectReads']['agentQueue']['feed']
}
const resourceAtomsBySource = new WeakMap<DataSource, ResourceAtoms>()

/** Retain authoritative reads only after explicit intent, never merely selecting an agent. */
export const retainedResourceAtoms = (source: DataSource): ResourceAtoms => {
  let atoms = resourceAtomsBySource.get(source)
  if (atoms === undefined) {
    atoms = {
      resources: Atom.family((ref: string) => Atom.make(get => get(source.resources?.byAgent(ref) ?? unsupportedResources)).pipe(Atom.setIdleTTL('60 seconds'))),
      queue: Atom.family((ref: string) => Atom.make(get => get(source.subjectReads.agentQueue.feed(ref))).pipe(Atom.setIdleTTL('60 seconds'))),
    }
    resourceAtomsBySource.set(source, atoms)
  }
  return atoms
}

/** Native conversation controls, retained from the former ConversationDetail header portal. */
export const ConversationHeaderActions = ({ agentRef, ux }: { readonly agentRef: string; readonly ux?: UxTelemetry }) => {
  const source = useDataSource()
  const registry = React.useContext(RegistryContext)
  const atoms = retainedResourceAtoms(source)
  const prefetch = () => {
    registry.mount(atoms.resources(agentRef))()
    registry.mount(atoms.queue(agentRef))()
  }
  const [showSystemEvents, setShowSystemEvents] = useAtom(systemEventsPreference)
  const [resourcesOpen, setResourcesOpen] = React.useState(false)
  const [opening, setOpening] = React.useState<OpenOperation>()
  const [cache, setCache] = React.useState<'retained' | 'cold'>('cold')
  const onOpenChange = (open: boolean) => {
    if (open) {
      setCache(registry.get(atoms.resources(agentRef))._tag === 'Observed' ? 'retained' : 'cold')
      setOpening(ux?.beginOpen({ surface: 'resources' }))
    }
    else { opening?.cancel(); setOpening(undefined) }
    setResourcesOpen(open)
  }
  const shellCommit = React.useCallback((node: HTMLElement | null) => {
    if (node === null || opening === undefined) return
    opening.phase('shell')
    return () => opening.cancel()
  }, [opening])
  const panelId = React.useId()
  /** Closing releases UI demand, but the intent-created wrapper retains its poll for 60 seconds. */
  const resources = useAtomValue(source.resources === undefined ? unsupportedResources
    : resourcesOpen ? atoms.resources(agentRef) : unopenedResources)
  const reasonId = React.useId()
  const disabledReason = resources._tag === 'Unavailable'
    ? resources.reason === 'ungranted' ? 'Read access to agent resources has not been granted.'
      : resources.reason === 'unsupported' ? 'This connection does not support agent resources.' : undefined
    : undefined
  // A non-modal complementary panel: opening it never inerts or hides the workspace, so the click
  // commits the panel in the same frame instead of restyling the whole app.
  return <div {...stylex.props(styles.actions)}>
    <Aria.Button aria-label="Resources" aria-expanded={resourcesOpen} aria-controls={resourcesOpen ? panelId : undefined} onHoverStart={prefetch} onFocus={prefetch} onPressStart={prefetch} onPress={() => onOpenChange(!resourcesOpen)} isDisabled={disabledReason !== undefined} aria-describedby={disabledReason === undefined ? undefined : reasonId} {...stylex.props(styles.button, styles.resourcesButton)}><CompositionIcon name="folder" /><span {...stylex.props(styles.resourcesLabel)}>Resources</span></Aria.Button>
    {resourcesOpen ? <SidePanel ref={shellCommit} id={panelId} aria-label="Agent resources" onClose={() => onOpenChange(false)}>
      <header {...stylex.props(styles.heading)}><strong>Resources</strong><Aria.Button aria-label="Close resources" onPress={() => onOpenChange(false)} {...stylex.props(styles.button)}>Close resources</Aria.Button></header>
      <ResourceContents agentRef={agentRef} resources={resources} opening={opening} cache={cache} />
    </SidePanel> : null}
    {disabledReason === undefined ? null : <span id={reasonId} {...stylex.props(styles.reason)}>{disabledReason}</span>}
    <Aria.MenuTrigger>
      <Aria.Button aria-label="Conversation menu" {...stylex.props(styles.button)}><CompositionIcon name="gear" /></Aria.Button>
      <Aria.Popover {...stylex.props(styles.popover)}>
        <Aria.Menu aria-label="Conversation settings" selectionMode="multiple" selectedKeys={showSystemEvents ? ['system'] : []} onSelectionChange={keys => setShowSystemEvents(keys === 'all' || keys.has('system'))} {...stylex.props(styles.menu)}>
          <Aria.MenuItem id="system" {...stylex.props(styles.menuItem)}>Show all system events</Aria.MenuItem>
        </Aria.Menu>
      </Aria.Popover>
    </Aria.MenuTrigger>
  </div>
}

const ResourceContents = ({ agentRef, resources, opening, cache }: {
  readonly agentRef: string
  readonly resources: Feed<ResourcePage>
  readonly opening?: OpenOperation
  readonly cache: 'retained' | 'cold'
}) => {
  const source = useDataSource()
  const queue = useAtomValue(retainedResourceAtoms(source).queue(agentRef))
  React.useLayoutEffect(() => {
    if (resources._tag !== 'Waiting') opening?.phase('data', { 'wf.ux.cache': cache })
    if (queue._tag !== 'Waiting') opening?.phase('queue')
    if (resources._tag !== 'Waiting' && queue._tag !== 'Waiting') return opening?.contentCommitted()
  }, [opening, cache, resources._tag, queue._tag])
  return <div {...stylex.props(styles.contents)}>
    <section aria-label="Todos">
      <h2 {...stylex.props(styles.sectionTitle)}>Todos</h2>
      <LiveAgentTodos agentRef={agentRef} placement="resources" />
    </section>
    <section aria-label="Agent queue">
      <h2 {...stylex.props(styles.sectionTitle)}>Agent queue</h2>
      {queue._tag !== 'Observed' ? <ReadNotice feed={queue} label="Agent queue" /> : <>
        {queue.freshness === 'stale' ? <p role="status">Showing the last verified agent queue while reconnecting.</p> : null}
        <p>{queue.value.current_work_ids.length} current work {queue.value.current_work_ids.length === 1 ? 'item' : 'items'}</p>
        {queue.value.current_work_ids.length === 0 ? null : <ul>{queue.value.current_work_ids.map(id => <li key={id}>{id}</li>)}</ul>}
        {queue.value.runs.length === 0 ? <p>No queued mission runs observed.</p> : <ol>{queue.value.runs.map(run => <li key={run.mission_run_id}>{run.mission_run_id} · {run.state} · {run.ready_work_ids.length} ready, {run.waiting_work_ids.length} waiting</li>)}</ol>}
      </>}
    </section>
    <section aria-label="Observed resources">
      <h2 {...stylex.props(styles.sectionTitle)}>Observed resources</h2>
      {resources._tag !== 'Observed' ? <ReadNotice feed={resources} label="Agent resources" /> : <>
        {resources.freshness === 'stale' ? <p role="status">Showing the last verified resources while reconnecting.</p> : null}
        {resources.value.items.length === 0 ? <p>No resources observed for this agent.</p> : <ul {...stylex.props(styles.list)}>{resources.value.items.map(resource => {
          const state = resourceState(resource)
          return <li key={resource.id} {...stylex.props(styles.resource)}>
            <div {...stylex.props(styles.resourceHeading)}><span {...stylex.props(styles.resourceTitle)}>{resourceTitle(resource)}</span>{state === undefined ? null : <span {...stylex.props(styles.resourceState)}>{state.replaceAll('_', ' ')}</span>}</div>
            <div {...stylex.props(styles.resourceMetadata)}><span>{resource.kind}</span><HumanTime at={DateTime.toEpochMillis(resource.observed_at)} kind="relative" aria-label="Resource observed" /></div>
          </li>
        })}</ul>}
        {resources.value.pagingError === undefined ? null : <p role="status" data-wf-resource-paging="failed">More resources could not be loaded; refresh to try again.</p>}
        {resources.value.nextCursor === null ? null : <Aria.Button isDisabled={resources.value.loadingMore === true} onPress={() => source.resources?.loadMore(agentRef)} {...stylex.props(styles.button)}>{resources.value.loadingMore ? 'Loading more resources…' : 'Load more resources'}</Aria.Button>}
      </>}
      {source.resources === undefined ? null : <Aria.Button onPress={() => source.resources?.refresh(agentRef)} {...stylex.props(styles.button)}>Refresh resources</Aria.Button>}
    </section>
  </div>
}

const ReadNotice = ({ feed, label }: { readonly feed: Exclude<Feed<unknown>, { readonly _tag: 'Observed' }>; readonly label: string }) =>
  <p role="status" data-wf-read-reason={feed._tag === 'Unavailable' ? feed.reason : undefined}>{feed._tag === 'Waiting' ? `Loading ${label.toLowerCase()}…` : feed.reason === 'ungranted' ? `Read access to ${label.toLowerCase()} has not been granted.` : feed.reason === 'unsupported' ? `This connection does not support ${label.toLowerCase()}.` : `${label} could not be loaded; reconnect and try again.`}</p>

const styles = stylex.create({
  actions: { display: 'flex', alignItems: 'center', gap: s.md },
  button: { display: 'inline-flex', alignItems: 'center', gap: s.xs, padding: '4px 8px', borderWidth: 0, borderRadius: 6, backgroundColor: 'transparent', color: c.fgMuted, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: c.rowHover }, ':focus-visible': { outline: `2px solid ${c.primary}` }, ':disabled': { opacity: 0.5, cursor: 'default' } },
  resourcesButton: { justifyContent: 'center', width: { default: 28, '@media (min-width: 640px)': 'auto' }, paddingInline: { default: 0, '@media (min-width: 640px)': s.md } },
  resourcesLabel: { display: { default: 'none', '@media (min-width: 640px)': 'inline' } },
  reason: { fontSize: t.denseSize, color: c.fgMuted },
  contents: { flex: '1 1 0', minHeight: 0, padding: s.lg, overflowY: 'auto' },
  heading: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: s.lg, borderBottom: `1px solid ${c.border}` },
  sectionTitle: { fontSize: t.uiSize, marginBlock: s.md },
  list: { listStyle: 'none', padding: 0 },
  resource: { padding: s.md, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6 },
  resourceHeading: { display: 'flex', alignItems: 'baseline', gap: s.md },
  resourceTitle: { flexGrow: 1, minWidth: 0, overflowWrap: 'anywhere' },
  resourceState: { flexShrink: 0, fontSize: t.metaSize, color: c.fgMuted },
  resourceMetadata: { display: 'flex', justifyContent: 'space-between', gap: s.md, fontSize: t.metaSize, color: c.fgMuted },
  popover: { backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6, fontFamily: t.fontSans, fontSize: t.metaSize, zIndex: 100 },
  menu: { padding: s.xs, outline: 'none' },
  menuItem: { padding: s.md, outline: 'none', borderRadius: 4, cursor: 'pointer', ':focus': { backgroundColor: c.rowHover }, ':is([data-selected])': { color: c.primary } },
})

import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { useAtom, useAtomValue } from '@effect/atom-react'
import * as Atom from 'effect/reactivity/Atom'
// Deep import: the assistant-ui barrel would pull the transcript graph into the eager shell.
import { Icon as CompositionIcon } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Icons.tsx'
import { colorVars as c, typeVars as t, spaceVars as s } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useDataSource } from '../data/react.tsx'
import { unavailable, type Feed } from '../data/source.ts'
import { resourceTitle, type ResourcePage } from '../resources/agent/model.ts'
import { systemEventsPreference } from './conversationPreferences.ts'

const unsupportedResources = Atom.make<Feed<ResourcePage>>(unavailable({ reason: 'unsupported', detail: '' }))

/** Native conversation controls, retained from the former ConversationDetail header portal. */
export const ConversationHeaderActions = ({ agentRef }: { readonly agentRef: string }) => {
  const source = useDataSource()
  const resources = useAtomValue(source.resources?.byAgent(agentRef) ?? unsupportedResources)
  const [showSystemEvents, setShowSystemEvents] = useAtom(systemEventsPreference)
  const [resourcesOpen, setResourcesOpen] = React.useState(false)
  const reasonId = React.useId()
  const disabledReason = resources._tag === 'Unavailable'
    ? resources.reason === 'ungranted' ? 'Read access to agent resources has not been granted.'
      : resources.reason === 'unsupported' ? 'This connection does not support agent resources.' : undefined
    : undefined
  return <div {...stylex.props(styles.actions)}>
    <Aria.DialogTrigger isOpen={resourcesOpen} onOpenChange={setResourcesOpen}>
      <Aria.Button aria-label="Resources" aria-expanded={resourcesOpen} isDisabled={disabledReason !== undefined} aria-describedby={disabledReason === undefined ? undefined : reasonId} {...stylex.props(styles.button)}><CompositionIcon name="folder" />Resources</Aria.Button>
      <Aria.ModalOverlay isDismissable {...stylex.props(styles.overlay)}>
        <Aria.Modal {...stylex.props(styles.modal)}>
          <Aria.Dialog aria-label="Agent resources" {...stylex.props(styles.dialog)}>
            <header {...stylex.props(styles.heading)}><strong>Resources</strong><Aria.Button aria-label="Close resources" onPress={() => setResourcesOpen(false)} {...stylex.props(styles.button)}>Close resources</Aria.Button></header>
            <ResourceContents agentRef={agentRef} resources={resources} />
          </Aria.Dialog>
        </Aria.Modal>
      </Aria.ModalOverlay>
    </Aria.DialogTrigger>
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

const ResourceContents = ({ agentRef, resources }: {
  readonly agentRef: string
  readonly resources: Feed<ResourcePage>
}) => {
  const source = useDataSource()
  const queue = useAtomValue(source.subjectReads.agentQueue.feed(agentRef))
  return <div {...stylex.props(styles.contents)}>
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
        {resources.value.items.length === 0 ? <p>No resources observed for this agent.</p> : <ul {...stylex.props(styles.list)}>{resources.value.items.map(resource => <li key={resource.id} {...stylex.props(styles.resource)}>{resourceTitle(resource)}</li>)}</ul>}
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
  reason: { fontSize: t.denseSize, color: c.fgMuted },
  overlay: { position: 'fixed', inset: 0, zIndex: 100, display: 'flex', justifyContent: 'flex-end', backgroundColor: 'rgba(0, 0, 0, 0.4)' },
  modal: { width: 380, maxWidth: 'calc(100vw - 24px)', height: '100%', backgroundColor: c.raised, color: c.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  dialog: { display: 'flex', flexDirection: 'column', height: '100%', outline: 'none' },
  heading: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: s.lg, borderBottom: `1px solid ${c.border}` },
  contents: { padding: s.lg, overflowY: 'auto' },
  sectionTitle: { fontSize: t.uiSize, marginBlock: s.md },
  list: { listStyle: 'none', padding: 0 },
  resource: { padding: s.md, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6 },
  popover: { backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6, fontFamily: t.fontSans, fontSize: t.metaSize, zIndex: 100 },
  menu: { padding: s.xs, outline: 'none' },
  menuItem: { padding: s.md, outline: 'none', borderRadius: 4, cursor: 'pointer', ':focus': { backgroundColor: c.rowHover }, ':is([data-selected])': { color: c.primary } },
})

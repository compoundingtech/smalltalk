import type { Attention } from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import type * as Atom from 'effect/reactivity/Atom'
import * as React from 'react'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'
import { RenderProfiler } from '../telemetry/meters.tsx'

import { attentionRefs } from '../data/projections.ts'
import { useDataSource } from '../data/react.tsx'
import type { Feed } from '../data/source.ts'
import type { MissionView } from '../missions/model.ts'
import { Tree, TreeNode } from '../workbench-kit/Tree.tsx'
import { AgentFolders } from './AgentFolders.tsx'
import { useOpen, useSubjects } from './context.tsx'
import type { Workspace, WindowAction } from './workspaces.ts'

/** The workbench's only navigation column: sessions, every attention card and ordinary resources. */
export const Sidebar = React.memo(
  ({
    selection,
    focusedRef,
    workspaces,
    missions,
    attention: attentionFeed,
    read,
    dispatch,
  }: {
    readonly selection: Atom.Atom<string>
    readonly focusedRef: string | null
    readonly workspaces: ReadonlyArray<Workspace>
    readonly missions: Feed<ReadonlyArray<MissionView>>
    readonly attention: Feed<readonly Attention[]>
    readonly read: ReadonlySet<string>
    readonly dispatch: (action: WindowAction) => void
  }) => {
    const agentWorkspaces = React.useMemo(
      () => workspaces.filter((workspace) => workspace.id.startsWith('agent/')),
      [workspaces],
    )
    const open = useOpen()
    const subjects = useSubjects()
    const source = useDataSource()
    const views = missions._tag === 'Observed' ? missions.value : []
    const attention = attentionFeed._tag === 'Observed' ? attentionFeed.value : []
    const inboxRefs = new Set<string>(attention.map((item) => item.id))
    for (const ref of attentionRefs(attention)) inboxRefs.add(ref)
    const inbox = [
      ...attention.map((item) => ({
        id: item.id,
        title: item.title,
        ref: item.mission_id ?? item.source_id,
        workspace: undefined as string | undefined,
        unread: !read.has(item.id),
        state: item.state,
      })),
      ...[...subjects.values()]
        .filter((subject) => subject.attention === true && !inboxRefs.has(subject.ref))
        .map((subject) => {
          const workspace = workspaces.find((candidate) =>
            candidate.notifications.some((notification) => notification.ref === subject.ref),
          )
          return {
            id: subject.ref,
            title: subject.title,
            ref: subject.ref,
            workspace: workspace?.id,
            unread: !read.has(subject.ref),
            state: 'open' as const,
          }
        }),
    ]
    const resources = [...subjects.values()].filter(
      (subject) =>
        !subject.ref.startsWith('agent/') &&
        !subject.ref.startsWith('terminal/') &&
        !subject.ref.startsWith('mission/') &&
        subject.attention !== true,
    )
    const groups = [
      {
        id: 'missions/you',
        title: 'Needs you',
        items: views.filter((view) => view.mission.must_act === 'you'),
      },
      {
        id: 'missions/running',
        title: 'Running',
        items: views.filter(
          (view) => view.mission.must_act !== 'you' && view.mission.state === 'running',
        ),
      },
      {
        id: 'missions/other',
        title: 'Standing & done',
        items: views.filter(
          (view) => view.mission.must_act !== 'you' && view.mission.state !== 'running',
        ),
      },
    ]
    const act = ({ id }: { readonly id: string }) => {
      const item = inbox.find((candidate) => `inbox/${candidate.id}` === id)
      if (item !== undefined) {
        dispatch({ _tag: 'ReadInbox', ref: item.id })
        if (item.workspace !== undefined) dispatch({ _tag: 'SelectWorkspace', id: item.workspace })
        open({ ref: item.ref })
        return
      }
      if (id.startsWith('subject/')) open({ ref: id.slice('subject/'.length) })
    }
    const current = focusedRef === null ? null : `subject/${focusedRef}`
    return (
      <nav aria-label="Workbench navigation" {...stylex.props(styles.sidebar)}>
        <header {...stylex.props(styles.heading)}>webfractal</header>
        <AgentFolders workspaces={agentWorkspaces} selection={selection} dispatch={dispatch} />
        <Tree
          label="Navigation"
          currentId={current}
          defaultExpandedKeys={['inbox']}
          onAction={(id) => act({ id })}
        >
          <TreeNode
            key="inbox"
            id="inbox"
            textValue="Inbox"
            content={
              <RenderProfiler id="inbox">
                <Section title="Inbox" count={inbox.filter((item) => item.unread).length} unread />
              </RenderProfiler>
            }
          >
            {inbox.length === 0 ? (
              <TreeNode
                id="inbox/empty"
                textValue="Nothing needs you"
                content={<span {...stylex.props(styles.meta)}>Nothing needs you</span>}
              />
            ) : (
              inbox.map((item) => (
                <TreeNode
                  key={item.id}
                  id={`inbox/${item.id}`}
                  textValue={item.title}
                  onHoverStart={() => {
                    if (item.ref.startsWith('agent/')) source.prefetchConversation?.(item.ref)
                  }}
                  onFocus={() => {
                    if (item.ref.startsWith('agent/')) source.prefetchConversation?.(item.ref)
                  }}
                  content={
                    <RenderProfiler id="inbox">
                      <span title={item.title} {...stylex.props(styles.name)}>
                        {item.title}
                      </span>
                      <span
                        role="img"
                        aria-label={`${item.unread ? 'Unread' : 'Read'} · ${item.state === 'open' ? 'Open' : 'Resolved'}`}
                        title={`${item.unread ? 'Unread' : 'Read'} · ${item.state === 'open' ? 'Open request' : 'Resolved request'}`}
                        {...stylex.props(styles.inboxState)}
                      >
                        {item.unread ? (
                          <span aria-hidden="true" {...stylex.props(styles.unreadDot)} />
                        ) : null}
                        <svg
                          aria-hidden="true"
                          width="12"
                          height="12"
                          viewBox="0 0 16 16"
                          fill="none"
                          stroke="currentColor"
                          strokeWidth="1.5"
                        >
                          <circle cx="8" cy="8" r="6" />
                          {item.state === 'open' ? (
                            <circle cx="8" cy="8" r="1" fill="currentColor" stroke="none" />
                          ) : (
                            <path d="m4.5 8 2.25 2.25 4.75-4.75" />
                          )}
                        </svg>
                      </span>
                    </RenderProfiler>
                  }
                />
              ))
            )}
          </TreeNode>
          <TreeNode
            key="missions"
            id="missions"
            textValue="Missions"
            content={<Section title="Missions" count={views.length} />}
          >
            {missions._tag !== 'Observed' ? (
              <TreeNode
                id="missions/status"
                textValue="Missions unavailable"
                content={
                  <span {...stylex.props(styles.meta)}>
                    {missions._tag === 'Waiting' ? 'Waiting for missions…' : missions.detail}
                  </span>
                }
              />
            ) : (
              groups.map((group) => (
                <TreeNode
                  key={group.id}
                  id={group.id}
                  textValue={group.title}
                  content={<Section title={group.title} count={group.items.length} />}
                >
                  {group.items.length === 0 ? (
                    <TreeNode
                      id={`${group.id}/empty`}
                      textValue="No missions"
                      content={<span {...stylex.props(styles.meta)}>No missions</span>}
                    />
                  ) : (
                    group.items.map((view) => (
                      <TreeNode
                        key={view.mission.id}
                        id={`subject/${view.mission.id}`}
                        textValue={subjects.get(view.mission.id)?.title ?? view.mission.title}
                        content={
                          <span title={view.mission.title} {...stylex.props(styles.name)}>
                            {subjects.get(view.mission.id)?.title ?? view.mission.title}
                          </span>
                        }
                      />
                    ))
                  )}
                </TreeNode>
              ))
            )}
          </TreeNode>
          <TreeNode
            key="resources"
            id="resources"
            textValue="Resources"
            content={<Section title="Resources" count={resources.length} />}
          >
            {resources.length === 0 ? (
              <TreeNode
                id="resources/empty"
                textValue="No resources"
                content={<span {...stylex.props(styles.meta)}>No resources</span>}
              />
            ) : (
              resources.map((subject) => (
                <TreeNode
                  key={subject.ref}
                  id={`subject/${subject.ref}`}
                  textValue={subject.title}
                  content={
                    <span
                      title={`${subject.title} · ${subject.detail ?? subject.ref}`}
                      {...stylex.props(styles.name)}
                    >
                      {subject.title}
                    </span>
                  }
                />
              ))
            )}
          </TreeNode>
        </Tree>
        <p {...stylex.props(styles.note)}>
          Reading clears unread, not an open request. Resolve requests in their source.
        </p>
      </nav>
    )
  },
)

const styles = stylex.create({
  sidebar: {
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minHeight: 0,
    overflowY: 'auto',
    backgroundColor: tokens['--ds-background-200'],
  },
  heading: {
    display: 'flex',
    alignItems: 'center',
    height: '1.75rem',
    flexShrink: 0,
    paddingInline: scale.space3,
    fontWeight: 600,
    borderBottomWidth: '1px',
    borderBottomStyle: 'solid',
    borderBottomColor: tokens['--ds-gray-alpha-400'],
  },
  section: { fontWeight: 500, flexGrow: 1 },
  name: {
    overflow: 'hidden',
    whiteSpace: 'nowrap',
    textOverflow: 'ellipsis',
    minWidth: 0,
    flexGrow: 1,
  },
  meta: { color: tokens['--ds-gray-900'], fontSize: '0.6875rem' },
  inboxState: {
    display: 'inline-flex',
    alignItems: 'center',
    gap: scale.space2,
    color: tokens['--ds-gray-900'],
    flexShrink: 0,
  },
  unreadDot: {
    width: '5px',
    height: '5px',
    borderRadius: scale.radiusFull,
    backgroundColor: tokens['--ds-blue-900'],
  },
  badge: {
    color: tokens['--ds-amber-900'],
    backgroundColor: tokens['--ds-amber-200'],
    borderRadius: scale.radiusFull,
    minWidth: '1.125rem',
    paddingInline: scale.space1,
    textAlign: 'center',
    fontSize: '0.6875rem',
  },
  count: { color: tokens['--ds-gray-900'], fontSize: '0.6875rem' },
  note: {
    paddingInline: scale.space3,
    paddingBlock: scale.space2,
    margin: 0,
    color: tokens['--ds-gray-900'],
    fontSize: '0.6875rem',
    lineHeight: 1.5,
  },
})

const Section = ({
  title,
  count,
  unread = false,
}: {
  readonly title: string
  readonly count: number
  readonly unread?: boolean
}) => (
  <>
    <span {...stylex.props(styles.section)}>{title}</span>
    <span {...stylex.props(unread && count > 0 ? styles.badge : styles.count)}>{count}</span>
  </>
)

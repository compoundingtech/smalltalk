// One pane in a workspace: an independently selected surface and its tab strip.
// Background surfaces are unmounted and hold no follows.

import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'

import { Menu, MenuItem, MenuPopover, MenuTrigger } from '../ui-compat/components.tsx'
import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import type { PresentationId, SubjectRendererManifest } from '../resources/contract.ts'
import { HarnessIcon } from '../workbench-kit/brand-icons.tsx'
import { EditorDropZone, EditorGroup, type GroupSurface } from '../workbench-kit/EditorGroup.tsx'
import { EditorTabs, type EditorTabItem } from '../workbench-kit/EditorTabs.tsx'
import { IconButton } from '../workbench-kit/IconButton.tsx'
import { CloseIcon, SplitDownIcon, SplitRightIcon } from '../workbench-kit/icons.tsx'
import { KeybindingHint, type Platform } from '../workbench-kit/keys.tsx'
import type { SubjectSummary } from './context.tsx'
import { ContainerIcon } from './icons.tsx'
import {
  subjectKey,
  groupOrder,
  type EditorEntry,
  type GroupId,
  type WorkbenchAction,
  type WorkbenchState,
} from './state.ts'

/** Surface strip: classic tabs, compact pills, title-bar-only label, vertical list. */
export type TabsVariant = 'classic' | 'compact' | 'title' | 'list'
/** MIME type every pane's tab strip and drop zone share, so surfaces move between panes. */
export const EDITOR_DRAG_TYPE = 'application/x-wf-editor'

const styles = stylex.create({
  titleBar: {
    display: 'flex',
    alignItems: 'center',
    gap: scale.space2,
    height: '2.25rem',
    flexShrink: 0,
    paddingInline: scale.space3,
    borderBottomWidth: '1px',
    borderBottomStyle: 'solid',
    borderBottomColor: tokens['--ds-gray-alpha-400'],
    fontSize: '0.8125rem',
    color: tokens['--ds-gray-1000'],
  },
  titleText: {
    fontWeight: 500,
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
  },
  titleDetail: { color: tokens['--ds-gray-900'], fontSize: '0.75rem', whiteSpace: 'nowrap' },
  titleCount: {
    color: tokens['--ds-gray-900'],
    fontSize: '0.75rem',
    marginInlineStart: 'auto',
    display: 'flex',
    alignItems: 'center',
    gap: scale.space2,
    whiteSpace: 'nowrap',
  },
  empty: {
    display: 'flex',
    flexDirection: 'column',
    alignItems: 'center',
    justifyContent: 'center',
    gap: scale.space3,
    flexGrow: 1,
    color: tokens['--ds-gray-900'],
    fontSize: '0.8125rem',
  },
  emptyRow: { display: 'flex', alignItems: 'center', gap: scale.space3 },
})

/** One pane of the current layout plus how to render its active editor. */
export interface GroupViewProps {
  readonly id: GroupId
  readonly state: WorkbenchState
  readonly dispatch: (action: WorkbenchAction) => void
  readonly tabs: TabsVariant
  readonly surface: GroupSurface
  readonly subjects: ReadonlyMap<string, SubjectSummary>
  readonly dirty: ReadonlySet<string>
  readonly platform: Platform
  readonly renderEditor: (entry: EditorEntry) => ReactNode
  readonly subjectManifests: ReadonlyArray<SubjectRendererManifest>
}

/** A pane: its surface strip (or title bar), split actions, drop zone and active editor. */
export const GroupView = ({
  id,
  state,
  dispatch,
  tabs,
  surface,
  subjects,
  dirty,
  platform,
  renderEditor,
  subjectManifests,
}: GroupViewProps) => {
  const group = state.groups[id] ?? { editors: [], active: null }
  const focused = state.focusedGroup === id
  const active = group.editors.find((entry) => subjectKey(entry.input) === group.active)
  const activeSubject = active === undefined ? undefined : subjects.get(active.input.ref)
  const label = `Pane ${groupOrder(state.editorArea).indexOf(id) + 1}`
  const focusThen = (action: WorkbenchAction) => {
    dispatch({ _tag: 'FocusGroup', group: id })
    dispatch(action)
  }
  const family = active?.input.ref.split('/')[0]
  const presentations = [
    ...new Set(
      subjectManifests
        .filter((manifest) => manifest.family === family || manifest.family === '*')
        .flatMap((manifest) => manifest.presentations),
    ),
  ]
  const actions = (
    <>
      {active === undefined ? null : (
        <MenuTrigger>
          <IconButton
            label="Choose presentation"
            icon={<ContainerIcon name="resources" size={14} />}
          />
          <MenuPopover>
            <Menu
              aria-label="Subject presentation"
              selectionMode="single"
              selectedKeys={[active.input.presentation]}
              onAction={(key) => {
                const presentation = presentations.find((candidate) => candidate === key)
                if (presentation !== undefined)
                  focusThen({ _tag: 'Open', input: { ref: active.input.ref, presentation } })
              }}
            >
              {presentations.map((presentation) => (
                <MenuItem key={presentation} id={presentation}>
                  {presentationTitle(presentation)}
                </MenuItem>
              ))}
            </Menu>
          </MenuPopover>
        </MenuTrigger>
      )}
      {active?.input.ref.startsWith('agent/') ? (
        <>
          <IconButton
            label="Open agent terminal"
            icon={<ContainerIcon name="terminal" size={14} />}
            shortcut={<KeybindingHint keys="mod+`" platform={platform} />}
            onPress={() => focusThen({ _tag: 'OpenTerminal' })}
          />
          <MenuTrigger>
            <IconButton label="Terminal options" icon={<SplitRightIcon />} />
            <MenuPopover>
              <Menu
                aria-label="Terminal placement"
                onAction={(key) => focusThen({ _tag: 'OpenTerminal', side: key === 'split' })}
              >
                <MenuItem id="tab">Open terminal as tab</MenuItem>
                <MenuItem id="split">Open terminal to the side</MenuItem>
              </Menu>
            </MenuPopover>
          </MenuTrigger>
        </>
      ) : null}
      <IconButton
        label="Split pane right"
        icon={<SplitRightIcon />}
        shortcut={<KeybindingHint keys={'mod+\\'} platform={platform} />}
        onPress={() => focusThen({ _tag: 'Split', dir: 'row' })}
      />
      <IconButton
        label="Split pane down"
        icon={<SplitDownIcon />}
        onPress={() => focusThen({ _tag: 'Split', dir: 'col' })}
      />
    </>
  )
  const body = (
    <EditorDropZone
      dragType={EDITOR_DRAG_TYPE}
      {...(tabs === 'title' || active === undefined ? { label: `${label} surface` } : {})}
      onDropEditor={({ source, zone }) =>
        zone === 'center'
          ? dispatch({
              _tag: 'Move',
              from: source.groupId,
              key: source.tabId,
              to: id,
              index: group.editors.length,
            })
          : dispatch({
              _tag: 'MoveToSplit',
              from: source.groupId,
              key: source.tabId,
              target: id,
              zone,
            })
      }
    >
      {active === undefined ? (
        <div {...stylex.props(styles.empty)}>
          <span>No surface open in this pane</span>
          <span {...stylex.props(styles.emptyRow)}>
            Go to subject <KeybindingHint keys="mod+p" platform={platform} />
          </span>
          <span {...stylex.props(styles.emptyRow)}>
            Show all commands <KeybindingHint keys="mod+shift+p" platform={platform} />
          </span>
        </div>
      ) : (
        renderEditor(active)
      )}
    </EditorDropZone>
  )

  return (
    <EditorGroup
      label={label}
      isFocused={focused}
      surface={surface}
      attention={activeSubject?.attention === true}
      onFocusWithin={() => {
        if (!focused) dispatch({ _tag: 'FocusGroup', group: id })
      }}
    >
      {tabs === 'title' ? (
        <>
          <div {...stylex.props(styles.titleBar)}>
            {active === undefined ? null : (
              <>
                <ContainerIcon name={activeSubject?.icon ?? 'resources'} size={14} />
                <span {...stylex.props(styles.titleText)}>
                  {activeSubject?.title ?? active.input.ref}
                </span>
                <span {...stylex.props(styles.titleDetail)}>
                  {activeSubject?.detail}
                  {activeSubject?.harness === undefined ? null : (
                    <HarnessIcon id={activeSubject.harness} />
                  )}
                </span>
              </>
            )}
            <span {...stylex.props(styles.titleCount)}>
              {group.editors.length > 1 ? `${group.editors.length - 1} more` : null}
              <KeybindingHint keys="mod+p" platform={platform} />
              {actions}
              {active === undefined ? null : (
                <IconButton
                  label="Close surface"
                  icon={<CloseIcon />}
                  onPress={() =>
                    dispatch({ _tag: 'Close', group: id, key: subjectKey(active.input) })
                  }
                />
              )}
            </span>
          </div>
          {body}
        </>
      ) : (
        <EditorTabs
          label={`${label} surfaces`}
          groupId={id}
          tabs={group.editors.map((entry) =>
            tabItem({
              entry,
              siblings: group.editors,
              subjects,
              dirty,
              alwaysDetail: tabs === 'list',
            }),
          )}
          activeId={group.active}
          variant={tabs === 'classic' ? 'classic' : tabs === 'compact' ? 'compact' : 'vertical'}
          isGroupFocused={focused}
          dragType={EDITOR_DRAG_TYPE}
          onActivate={(key) => dispatch({ _tag: 'Activate', group: id, key })}
          onClose={(key) => dispatch({ _tag: 'Close', group: id, key })}
          onMove={({ source, toIndex }) =>
            dispatch({
              _tag: 'Move',
              from: source.groupId,
              key: source.tabId,
              to: id,
              index: toIndex,
            })
          }
          actions={actions}
        >
          {body}
        </EditorTabs>
      )}
    </EditorGroup>
  )
}

const tabItem = ({
  entry,
  siblings,
  subjects,
  dirty,
  alwaysDetail,
}: {
  readonly entry: EditorEntry
  readonly siblings: ReadonlyArray<EditorEntry>
  readonly subjects: ReadonlyMap<string, SubjectSummary>
  readonly dirty: ReadonlySet<string>
  readonly alwaysDetail: boolean
}): EditorTabItem => {
  const subject = subjects.get(entry.input.ref)
  const name = subject?.title ?? entry.input.ref.slice(entry.input.ref.lastIndexOf('/') + 1)
  const title =
    entry.input.presentation !== 'detail'
      ? `${name} · ${presentationTitle(entry.input.presentation)}`
      : subject?.icon === 'conversation'
        ? `${name} · Chat`
        : subject?.icon === 'terminal'
          ? `${name} · Terminal`
          : (subject?.title ?? entry.input.ref)
  // Equal agent names on different hosts need their existing contextual detail.
  const showDetail =
    alwaysDetail ||
    siblings.some(
      (other) =>
        other !== entry && (subjects.get(other.input.ref)?.title ?? other.input.ref) === name,
    )
  const key = subjectKey(entry.input)
  return {
    id: key,
    title,
    icon: <ContainerIcon name={subject?.icon ?? 'resources'} size={14} />,
    ...(showDetail && subject?.detail !== undefined ? { detail: subject.detail } : {}),
    ...(showDetail && subject?.harness !== undefined ? { harness: subject.harness } : {}),
    ...(dirty.has(key) ? { dirty: true } : {}),
    ...(subject?.status === undefined ? {} : { status: subject.status }),
    ...(subject?.attention === true ? { attention: true } : {}),
  }
}

const presentationTitle = (presentation: PresentationId): string =>
  presentation.slice(0, 1).toUpperCase() + presentation.slice(1)

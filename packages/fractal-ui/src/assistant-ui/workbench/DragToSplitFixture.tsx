import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { useDrag } from 'react-aria'
import { Button, Menu, MenuItem, MenuTrigger, Popover } from 'react-aria-components'
import { Workbench } from './Workbench'
import { AGENT_DRAG_MIME, openAgentAtPath, type AgentPlacement } from './agent-drag'
import { findGroupPath, group, split, type WorkbenchLayout } from './workbench-model'
import { decidedAppearance, useWorkbenchFixture } from './workbench-fixtures'
import { surfaceVars as sf, textVars as tx, borderVars as bd, accentVars as ac, spaceVars as s, typeVars as t, geometryVars as g, radiusVars as r } from '../composition-tokens.stylex'
import { lightTheme } from '../composition-theme'
import { Icon } from '../composition/Icons'

const draggedAgent = 'agent:worker-2'
const mainGroup = group([{ uri: 'agent:worker-1' }])
const pair = split('right', mainGroup, group([{ uri: 'diff:sample/rows.ts', form: 'unified' }]))

export function AgentDragRow({ onOpen, agentKey = draggedAgent, title = 'worker-2' }: { onOpen: (placement: AgentPlacement) => void; agentKey?: string; title?: string }) {
  const getItems = React.useCallback(() => [{ [AGENT_DRAG_MIME]: agentKey, 'text/plain': agentKey }], [agentKey])
  const { dragProps } = useDrag({ getItems, getAllowedDropOperations: () => ['copy'] })
  return (
    <div {...stylex.props(styles.row)}>
      <button {...dragProps} aria-label={`Drag ${title}`} data-testid="drag-agent" data-agent-key={agentKey} {...stylex.props(styles.agent)}>
        <Icon name="message" /><span {...stylex.props(styles.agentTitle)}>{title}</span>
      </button>
      <MenuTrigger>
        <Button aria-label={`${title} actions`} {...stylex.props(styles.actions)}><Icon name="chevron-down" /></Button>
        <Popover placement="bottom end" {...stylex.props(styles.popover)}>
          <Menu aria-label={`Open ${title}`} onAction={key => onOpen(key as AgentPlacement)} {...stylex.props(styles.menu)}>
            <MenuItem id="center" {...stylex.props(styles.item)}>Open as tab</MenuItem>
            <MenuItem id="right" {...stylex.props(styles.item)}>Open in split right</MenuItem>
            <MenuItem id="below" {...stylex.props(styles.item)}>Open in split below</MenuItem>
            <MenuItem id="left" {...stylex.props(styles.item)}>Open in split left</MenuItem>
            <MenuItem id="above" {...stylex.props(styles.item)}>Open in split above</MenuItem>
          </Menu>
        </Popover>
      </MenuTrigger>
    </div>
  )
}

/** A scoped drag toolbar exercises the native drag key and the host layout commands. */
export function DragToSplitFixture({ scheme = 'dark', groups = 2 }: { scheme?: 'dark' | 'light'; groups?: 1 | 2 }) {
  const { resources, describePane } = useWorkbenchFixture()
  const [layout, setLayout] = React.useState<WorkbenchLayout>(groups === 1 ? mainGroup : pair)
  const [focused, setFocused] = React.useState('agent:worker-1')
  const open = (placement: AgentPlacement) => {
    const path = findGroupPath(layout, focused.split(' ')[0]) ?? findGroupPath(layout)
    if (path !== undefined) {
      setLayout(openAgentAtPath(layout, path, draggedAgent, placement))
      setFocused(draggedAgent)
    }
  }
  return (
    <main aria-label="Drag-to-split workspace" data-testid="drag-workspace" data-layout={JSON.stringify(layout)} {...stylex.props(styles.root, ...(scheme === 'light' ? lightTheme : []))}>
      <div role="toolbar" aria-label="Open agent" {...stylex.props(styles.toolbar)}>
        <header {...stylex.props(styles.heading)}>Workspace</header>
        <AgentDragRow onOpen={open} />
      </div>
      <Workbench layout={layout} resources={resources} appearance={decidedAppearance} describePane={describePane} workspaceId={`drag-story-${groups}`} scheme={scheme} onLayoutChange={setLayout} focusedPaneKey={focused} onPaneSelect={setFocused} />
    </main>
  )
}

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: '100vh', minHeight: 0, minWidth: 0, overflow: 'hidden', fontFamily: t.fontSans, backgroundColor: sf.canvas, color: tx.fg },
  toolbar: { display: 'flex', alignItems: 'center', gap: s.md, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: bd.border, padding: s.sm, boxSizing: 'border-box' },
  heading: { display: 'flex', alignItems: 'center', height: g.band, paddingInline: s.sm, fontSize: t.metaSize, fontWeight: t.weightMedium },
  row: { display: 'flex', alignItems: 'center', gap: s.xs2 },
  agent: { display: 'flex', alignItems: 'center', gap: s.sm, flexGrow: 1, height: g.controlLg, paddingInline: s.sm, borderWidth: 0, borderRadius: r.control, backgroundColor: sf.transparent, color: tx.fg, fontSize: t.metaSize, cursor: 'grab', ':hover': { backgroundColor: sf.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary } },
  agentTitle: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  actions: { display: 'flex', alignItems: 'center', justifyContent: 'center', width: g.controlMd, height: g.controlMd, borderWidth: 0, borderRadius: r.control, backgroundColor: sf.transparent, color: tx.fgMuted, ':hover': { backgroundColor: sf.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary } },
  popover: { backgroundColor: sf.raised, color: tx.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: bd.border, borderRadius: r.md },
  menu: { display: 'flex', flexDirection: 'column', padding: s.xs2, outlineStyle: 'none', fontFamily: t.fontSans, fontSize: t.metaSize },
  item: { paddingInline: s.sm, paddingBlock: s.xs, borderRadius: r.sm, cursor: 'pointer', outlineStyle: 'none', ':focus': { backgroundColor: sf.rowHover } },
})

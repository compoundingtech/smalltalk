import { addTabAtPath, nodeAtPath, parsePaneKey, replaceAtPath, splitGroupAtPath, type LayoutPath, type WorkbenchLayout } from './workbench-model'

/** Text payload: the canonical agent resource key, including optional pane form/view. */
export const AGENT_DRAG_MIME = 'application/x-wf-fractal-agent'
export type AgentPlacement = 'center' | 'right' | 'below' | 'left' | 'above'

/** Native subject refs become the established workspace resource URI, retaining form/view. */
export const agentPaneKey = (key: string): string => key.startsWith('agent/') ? `agent:${key.slice('agent/'.length)}` : key

/** Shared by canvas drops and the agent row's keyboard-accessible split commands. */
export function openAgentAtPath(layout: WorkbenchLayout, path: LayoutPath, agentKey: string, placement: AgentPlacement): WorkbenchLayout {
  if (agentKey.trim() === '') return layout
  const pane = parsePaneKey(agentPaneKey(agentKey))
  if (placement === 'center') return addTabAtPath(layout, path, pane)
  const axis = placement === 'left' ? 'right' : placement === 'above' ? 'below' : placement
  const next = splitGroupAtPath(layout, path, axis, pane)
  if (next === layout) return layout
  if (placement !== 'left' && placement !== 'above') return next
  const node = nodeAtPath(next, path)
  return node?.kind === 'split' ? replaceAtPath(next, path, { ...node, children: [node.children[1], node.children[0]] }) : next
}

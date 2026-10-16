/** One fixture feeds the sidebar rows, the switchers and the probe, so their counts always agree. */
export type AgentStatus = 'Running' | 'Finished' | 'Failed' | 'Needs you'
export interface Agent { readonly id: string; readonly name: string; readonly parent?: string; readonly status: AgentStatus; readonly unread: number; readonly task: string }
/** Reported scenarios first, then the samples that st's read model cannot back yet. */
export const scenarios = ['zero', 'one', 'running', 'many', 'unknown', 'failed', 'attention', 'alerts', 'nested', 'finished'] as const
export type Scenario = typeof scenarios[number]
/**
 * `reporting` is a harness capability, never inferred from an empty child list: an unreported harness has an unknown count, not zero.
 * `sample` marks child states the ledger does not report today (failed, needs you, finished, nesting); they stay labelled until #2288 ships.
 */
export interface WorkspaceFixture { readonly state: Scenario; readonly label: string; readonly harness: string; readonly reporting: 'reported' | 'unreported'; readonly sample: boolean; readonly agents: readonly Agent[] }

const names = ['Layout', 'Keyboard', 'Coverage', 'Copy', 'Routing', 'Storage', 'History', 'Focus', 'Overflow', 'Messages', 'Resources', 'Review', 'Delivery', 'Recovery']
/** Which child carries the scenario's alert; `alerts` puts both alerts at the end of a long list, where rails and tabs overflow. */
function childStatus(state: Scenario, index: number, count: number): AgentStatus {
  if (state === 'finished') return 'Finished'
  if (state === 'alerts') return index === count - 2 ? 'Failed' : index === count - 1 ? 'Needs you' : 'Running'
  if (index > 0) return 'Running'
  return state === 'failed' ? 'Failed' : state === 'attention' ? 'Needs you' : 'Running'
}
function workspace(state: Scenario, label: string, count: number, options: { harness?: string; reporting?: 'reported' | 'unreported'; sample?: boolean } = {}): WorkspaceFixture {
  const root: Agent = { id: 'root', name: label, status: 'Running', unread: 0, task: 'Coordinate the workspace navigation changes.' }
  const children = names.slice(0, count).map((name, index): Agent => {
    const status = childStatus(state, index, count)
    return { id: `child-${index + 1}`, name, parent: 'root', status, unread: status === 'Needs you' ? 1 : state === 'attention' ? 3 : 0, task: `Check ${name.toLowerCase()} behaviour in the workspace navigation.` }
  })
  const nested: Agent[] = state === 'nested' ? [
    { id: 'grandchild', name: 'Key map', parent: 'child-1', status: 'Running', unread: 2, task: 'Check the key map delegated by Layout.' },
    { id: 'great-grandchild', name: 'Shortcut audit', parent: 'grandchild', status: 'Finished', unread: 1, task: 'Audit the shortcuts delegated by Key map.' },
  ] : []
  return { state, label, harness: options.harness ?? 'Claude Code', reporting: options.reporting ?? 'reported', sample: options.sample ?? false, agents: [root, ...children, ...nested] }
}
export const workspaces: Record<Scenario, WorkspaceFixture> = {
  zero: workspace('zero', 'Notes', 0),
  one: workspace('one', 'Navigation', 1),
  running: workspace('running', 'Workspace', 3),
  many: workspace('many', 'Release', 14),
  unknown: workspace('unknown', 'Research', 0, { harness: 'OMP', reporting: 'unreported' }),
  failed: workspace('failed', 'Diagnostics', 3, { sample: true }),
  attention: workspace('attention', 'Decisions', 3, { sample: true }),
  alerts: workspace('alerts', 'Triage', 14, { sample: true }),
  nested: workspace('nested', 'Key map review', 3, { sample: true }),
  finished: workspace('finished', 'Completed review', 3, { sample: true }),
}
export const sidebarFixtures: readonly WorkspaceFixture[] = scenarios.map(state => workspaces[state])

const subagents = (fixture: WorkspaceFixture) => fixture.agents.filter(agent => agent.parent !== undefined)
/** Active means running or waiting on the operator; finished and failed subagents are not active. */
export const activeSubagents = (fixture: WorkspaceFixture) => subagents(fixture).filter(agent => agent.status === 'Running' || agent.status === 'Needs you').length
export const failedSubagents = (fixture: WorkspaceFixture) => subagents(fixture).filter(agent => agent.status === 'Failed').length
export const waitingSubagents = (fixture: WorkspaceFixture) => subagents(fixture).filter(agent => agent.status === 'Needs you').length
/** The count control's accessible name, in words. For reported counts the visible count text is always its start. */
export function activeCountName(fixture: WorkspaceFixture): string {
  if (fixture.reporting === 'unreported') return `Subagent count unavailable for ${fixture.label}: ${fixture.harness} does not report subagents`
  const active = activeSubagents(fixture)
  return [`${active} active ${active === 1 ? 'subagent' : 'subagents'}`, ...alertWords(subagents(fixture))].join(', ')
}
/** "1 failed", "2 need you": the spoken half of an alert cue, shared by count controls and overflow controls. */
export function alertWords(agents: readonly Agent[]): string[] {
  const failed = agents.filter(agent => agent.status === 'Failed').length
  const waiting = agents.filter(agent => agent.status === 'Needs you').length
  return [...(failed > 0 ? [`${failed} failed`] : []), ...(waiting > 0 ? [`${waiting} ${waiting === 1 ? 'needs' : 'need'} you`] : [])]
}
export const unreportedExplanation = (fixture: WorkspaceFixture) => `Subagents are not reported by ${fixture.harness}. This does not mean there are none.`
export const depthOf = (agent: Agent, agents: readonly Agent[]): number => agent.parent === undefined ? 0 : 1 + depthOf(agents.find(candidate => candidate.id === agent.parent)!, agents)

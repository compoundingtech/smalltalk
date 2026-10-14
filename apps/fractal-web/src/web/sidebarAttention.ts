import type { Agent } from '../data/source.ts'
import type { SubjectSummary } from '../shell/context.tsx'
import type { SidebarNode } from '../folders/sidebarContract.ts'

/** Native ask=question is actionable even when no independent attention card exists. */
const pendingAskAgents = (agents: readonly Agent[]): readonly Agent[] =>
  agents.filter(agent => agent.ask._tag === 'Known' && agent.ask.value === 'question')

/** Count agents, not attention cards or missions. Unknown attention never requests action. */
export const sidebarAttention = ({ agents, subjects }: {
  readonly agents: readonly Agent[]
  readonly subjects: readonly SubjectSummary[]
}): ReadonlySet<string> => {
  const agentRefs = new Set(agents.map(agent => agent.ref))
  const refs = new Set(subjects.filter(subject => subject.icon === 'conversation' &&
    subject.attention === true && agentRefs.has(subject.ref)).map(subject => subject.ref))
  for (const agent of pendingAskAgents(agents)) refs.add(agent.ref)
  return refs
}

/** A subject can occur more than once; rollups count the same distinct agents as the title. */
export const folderAttentionCount = (tree: readonly SidebarNode[], refs: ReadonlySet<string>): number => {
  const found = new Set<string>()
  const visit = (nodes: readonly SidebarNode[]): void => {
    for (const node of nodes) {
      if (node._tag === 'Agent') { if (refs.has(node.subject)) found.add(node.subject) }
      else visit(node.children)
    }
  }
  visit(tree)
  return found.size
}

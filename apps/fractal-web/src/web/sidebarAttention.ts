import type { Agent } from '../data/source.ts'
import type { SubjectSummary } from '../shell/context.tsx'

/** Count agents, not attention cards or missions. Unknown attention never requests action. */
export const sidebarAttention = ({ agents, subjects }: {
  readonly agents: readonly Agent[]
  readonly subjects: readonly SubjectSummary[]
}): ReadonlySet<string> => {
  const agentRefs = new Set(agents.map(agent => agent.ref))
  return new Set(subjects.filter(subject => subject.icon === 'conversation' &&
    subject.attention === true && agentRefs.has(subject.ref)).map(subject => subject.ref))
}

import type { Agent } from '../../clients/typescript/st3-client';

export type AgentRow = { agent: Agent; depth: number };

export function agentLabel(agent: Agent): string {
  const slug = agent.name.split('/').filter(Boolean).pop() ?? agent.id.split('/').filter(Boolean).pop() ?? agent.id;
  return slug.split('-').map(word => {
    const known: Record<string, string> = { st3: 'ST3', cos: 'COS', ios: 'iOS', tui: 'TUI', pty: 'PTY', api: 'API' };
    return known[word.toLowerCase()] ?? word.charAt(0).toUpperCase() + word.slice(1);
  }).join(' ');
}

export function agentTree(agents: Agent[] = []): AgentRow[] {
  const ids = new Set(agents.map(agent => agent.id));
  const rows: AgentRow[] = [];
  const seen = new Set<string>();
  function add(agent: Agent, depth: number) {
    if (seen.has(agent.id)) return;
    seen.add(agent.id);
    rows.push({ agent, depth });
    for (const child of agents.filter(candidate => candidate.under?.some(parent => parent.agent_id === agent.id))) add(child, depth + 1);
  }
  for (const agent of agents) if (!agent.under?.some(parent => ids.has(parent.agent_id))) add(agent, 0);
  for (const agent of agents) add(agent, 0);
  return rows;
}

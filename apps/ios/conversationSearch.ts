// What was said in conversations, as st's search finds it: the rows the Agents search shows
// under "said in conversations", as stui's Ctrl+K does. Only hits in an agent's conversation
// open; st's freshness notes ride along, so a missing match is not read as "never said".
import type { Agent, ConversationSearch } from '../../clients/typescript/st3-client';
import { agentName } from './agentsView';

export type SaidRow = { key: string; agentId: string; name: string; excerpt: string; when: string };

/** The rows for a search's hits, with the note st's freshness asks for (or ''). */
export function saidRows(found: ConversationSearch, agents: Agent[]): { rows: SaidRow[]; note: string } {
  const rows = found.items
    .filter(hit => hit.agent_id)
    .slice(0, 12)
    .map(hit => {
      const agent = agents.find(candidate => candidate.id === hit.agent_id);
      return {
        key: `${hit.conversation_id}:${hit.entry_id}`,
        agentId: hit.agent_id!,
        name: agent ? agentName(agent) : hit.agent_id!.replace(/^agent\//, ''),
        excerpt: hit.excerpt.split(/\s+/).filter(Boolean).join(' '),
        when: hit.timestamp.slice(5, 16).replace('T', ' '),
      };
    });
  const note = found.refreshing ? 'still indexing, more may come'
    : found.incomplete_sources.length ? 'some conversations could not be searched' : '';
  return { rows, note };
}

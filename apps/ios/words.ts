// Words, explanations and glyphs every client uses, pinned by fixtures/clients/words.json.
// The working glyph is the first frame of stui's spinner; a client may animate it.

import type { AgentState, AttentionKind, Harness, StepState, Tier, Word } from './clientView';
import type { ColorToken } from './theme';

export const SPINNER = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] as const;
const working = SPINNER[0];

export const TABS = ['Home', 'Agents', 'Missions', 'Fleet', 'Worktrees'] as const;
export type TabName = typeof TABS[number];

type WordInfo = { id: Word; name: string; explain: string; glyph: string; color: ColorToken };
/** Mission words in the order the Missions tab groups them. */
export const MISSION_WORDS: readonly WordInfo[] = [
  { id: 'decision', name: 'needs you', explain: 'a step is waiting for your answer', glyph: '◆', color: 'person' },
  { id: 'stalled', name: 'stalled', explain: 'a step has an owner who is not moving', glyph: '▲', color: 'fault' },
  { id: 'unstaffed', name: 'unstaffed', explain: 'a step is ready but its agent is stopped or broken', glyph: '◇', color: 'waiting' },
  { id: 'unclaimed', name: 'unclaimed', explain: 'a step is ready; st has not said which agent takes it', glyph: '◇', color: 'overlay1' },
  { id: 'queued', name: 'queued', explain: 'a step is waiting its turn on a busy agent; nothing to do', glyph: '◌', color: 'overlay1' },
  { id: 'working', name: 'working', explain: 'an agent is doing a step now', glyph: working, color: 'working' },
  { id: 'watching', name: 'watching', explain: 'st keeps this open and starts other missions when something happens', glyph: '◉', color: 'sapphire' },
  { id: 'held', name: 'held', explain: 'waiting on something outside the fleet', glyph: '◐', color: 'sapphire' },
  { id: 'idle', name: 'idle', explain: 'running, with nothing ready', glyph: '●', color: 'idle' },
  { id: 'done', name: 'done', explain: 'every step finished', glyph: '✓', color: 'done' },
  { id: 'failed', name: 'failed', explain: 'a step failed and nothing retried it', glyph: '✕', color: 'fault' },
];

type AgentInfo = { id: AgentState; name: string; glyph: string; color: ColorToken };
export const AGENT_STATES: readonly AgentInfo[] = [
  { id: 'needs_you', name: 'needs you', glyph: '◆', color: 'person' },
  { id: 'fault', name: 'broken', glyph: '✕', color: 'fault' },
  { id: 'working', name: 'working', glyph: working, color: 'working' },
  { id: 'idle', name: 'idle', glyph: '●', color: 'idle' },
  { id: 'starting', name: 'starting', glyph: '◌', color: 'waiting' },
  { id: 'stopped', name: 'stopped', glyph: '○', color: 'quiet' },
  { id: 'unknown', name: 'not managed', glyph: '?', color: 'quiet' },
];

export const TIERS: readonly { id: Tier; title: string }[] = [
  { id: 'stopped', title: 'somebody is stopped on you' },
  { id: 'alert', title: 'something broke' },
  { id: 'today', title: 'today' },
  { id: 'later', title: 'when there is time' },
];

export const wordInfo = (word: Word) => MISSION_WORDS.find(info => info.id === word)!;
export const agentInfo = (state: AgentState) => AGENT_STATES.find(info => info.id === state)!;
export const tierTitle = (tier: Tier) => TIERS.find(info => info.id === tier)!.title;
export const wordRank = (word: Word) => MISSION_WORDS.findIndex(info => info.id === word);
export const agentRank = (state: AgentState) => AGENT_STATES.findIndex(info => info.id === state);
export const tierRank = (tier: Tier) => TIERS.findIndex(info => info.id === tier);

export function stepStyle(state: StepState): { glyph: string; color: ColorToken; word: string } {
  switch (state) {
    case 'done': return { glyph: '▰', color: 'done', word: 'done' };
    case 'working': return { glyph: working, color: 'working', word: 'working' };
    case 'ready': return { glyph: '▱', color: 'waiting', word: 'ready' };
    case 'waiting': return { glyph: '◐', color: 'sapphire', word: 'waiting' };
    case 'needs_you': return { glyph: '◆', color: 'person', word: 'needs you' };
    case 'failed': return { glyph: '✕', color: 'fault', word: 'failed' };
    case 'pending': return { glyph: '▱', color: 'surface2', word: 'later' };
  }
}

export function attentionStyle(kind: AttentionKind['kind']): { glyph: string; color: ColorToken } {
  switch (kind) {
    case 'fault': return { glyph: '✕', color: 'fault' };
    case 'message': return { glyph: '✉', color: 'sapphire' };
    default: return { glyph: '◆', color: 'person' };
  }
}

export function harnessColor(harness: Harness): ColorToken {
  switch (harness) {
    case 'claude': return 'peach';
    case 'codex': return 'blue';
    case 'omp': return 'teal';
    case 'pi': return 'lavender';
    case 'unknown': return 'overlay0';
  }
}

/** What words.json holds: the contract view of this module. */
export function contractWords() {
  return {
    mission: MISSION_WORDS.map(({ id, name, explain, glyph }) => ({ explain, glyph, id, name })),
    agent: AGENT_STATES.map(({ id, name, glyph }) => ({ glyph, id, name })),
    tier: TIERS.map(({ id, title }) => ({ id, title })),
    tabs: [...TABS],
  };
}

import type { Agent, Attention, Mission, MissionStep, WorkState } from '../../clients/typescript/st3-client';
import { ago } from './presentation';
import { missionSteps } from './presentation';
import { theme } from './theme';

// The Missions tab, drawn the way stui draws it (crates/stui/src/ui/screens.rs missions_list,
// adapt.rs missions): one word per mission naming who has to move, in this order.
export const WORDS = ['decision', 'stalled', 'unstaffed', 'unclaimed', 'queued', 'working', 'watching', 'held', 'idle', 'done', 'failed', 'cancelled'] as const;
export type Word = typeof WORDS[number];

export function wordName(word: Word): string {
  return word === 'decision' ? 'needs you' : word;
}

export function wordStyle(word: Word, spinner = '⠿'): { glyph: string; color: string } {
  switch (word) {
    case 'decision': return { glyph: '◆', color: theme.person };
    case 'stalled': return { glyph: '▲', color: theme.fault };
    case 'unstaffed': return { glyph: '◇', color: theme.waiting };
    case 'unclaimed': return { glyph: '◇', color: theme.overlay1 };
    case 'queued': return { glyph: '◌', color: theme.overlay1 };
    case 'working': return { glyph: spinner, color: theme.working };
    case 'watching': return { glyph: '◉', color: theme.sapphire };
    case 'held': return { glyph: '◐', color: theme.sapphire };
    case 'idle': return { glyph: '●', color: theme.idle };
    case 'done': return { glyph: '✓', color: theme.done };
    case 'failed': return { glyph: '✕', color: theme.fault };
    case 'cancelled': return { glyph: '⊘', color: theme.quiet };
  }
}

export const MISSION_LEGEND: ReadonlyArray<Word> = ['decision', 'stalled', 'unstaffed', 'queued', 'watching', 'working', 'idle', 'done'];

/** Whether an agentless step only holds its run open, by the names the fleet uses for them. */
export function keepsOpen(path: string): boolean {
  const name = path.split('/').pop() ?? path;
  return ['keep-watch', 'retire', 'steward-intake', 'standing', 'keep-open'].includes(name) || name.endsWith('-retirement');
}

type QueueAgent = Pick<Agent, 'state' | 'fault' | 'next_work_id' | 'upcoming_work_ids'>;
function queuedFor<A extends QueueAgent>(agents: A[], work: string): A | undefined {
  return agents.find(agent => agent.next_work_id === work || (agent.upcoming_work_ids ?? []).includes(work));
}

// Keep cached projections readable while the gateway moves to the contract states.
function stepState(state: string): string {
  if (state === 'working' || state === 'running') return 'claimed';
  return state === 'pending' ? 'waiting' : state;
}
function activeStep(state: string): boolean {
  return state === 'claimed' || state === 'verifying';
}

export function missionWord(mission: Pick<Mission, 'id' | 'state' | 'run_details'>, attention: Array<Pick<Attention, 'attention_kind' | 'mission_id' | 'state'>>, agents: QueueAgent[]): Word {
  const work: MissionStep[] = missionSteps(mission);
  const states = work.map(step => stepState(step.state));
  const state = mission.state as string;
  if (attention.some(item => item.state !== 'resolved' && item.attention_kind === 'human-gate' && item.mission_id === mission.id)) return 'decision';
  if (state === 'blocked' || states.includes('blocked')) return 'stalled';
  if (state === 'completed') return 'done';
  if (state === 'failed' || states.includes('failed')) return 'failed';
  if (state === 'cancelled') return 'cancelled';
  if (work.length && work.every(step => step.state === 'completed' || (activeStep(stepState(step.state)) && step.agentless && keepsOpen(step.path))) && work.some(step => step.state !== 'completed')) return 'watching';
  if (states.some(activeStep)) return 'working';
  const ready = work.find(step => step.state === 'ready' && !step.claimant);
  if (ready) {
    const agent = queuedFor(agents, ready.id);
    if (!agent) return 'unclaimed';
    return agent.state === 'failed' || agent.state === 'stopped' || agent.fault ? 'unstaffed' : 'queued';
  }
  if (states.includes('waiting')) return 'held';
  if (['standing', 'running', 'ready', 'draft'].includes(state)) return 'idle';
  return 'done';
}

const labelWords: Record<string, string> = { tui: 'TUI', ios: 'iOS', st3: 'ST', omp: 'OMP', api: 'API', pty: 'PTY' };
/** `scope · Title`, as stui titles a mission. */
export function missionTitle(mission: Pick<Mission, 'id' | 'title'>): string {
  const path = mission.id.replace(/^mission\//, '');
  const slash = path.lastIndexOf('/');
  const scope = slash >= 0 ? path.slice(0, slash) : path;
  const slug = mission.title.split('/').pop() ?? mission.title;
  const name = slug.split('-').map(word => labelWords[word.toLowerCase()] ?? word.charAt(0).toUpperCase() + word.slice(1)).join(' ');
  return `${scope} · ${name}`;
}

/** st's own loop rounds and CI plumbing, folded away until the person asks. */
export function isSystemMission(id: string): boolean {
  return id.startsWith('mission/__st3/') || id.split('/').includes('ci');
}

export type MissionRow = { mission: Mission; word: Word; title: string; path: string; age: string; done: number; total: number; system: boolean; progress?: string };
export function missionRows(missions: Mission[], attention: Attention[], agents: Agent[], showSystem: boolean, now = Date.now()): { rows: MissionRow[]; hidden: number } {
  const all = missions.map((mission): MissionRow => {
    const steps = missionSteps(mission);
    return {
      mission,
      word: missionWord(mission, attention, agents),
      title: missionTitle(mission),
      path: mission.id.replace(/^mission\//, ''),
      age: ago(mission.updated_at, now),
      done: steps.filter(step => step.state === 'completed').length,
      total: steps.length,
      system: isSystemMission(mission.id),
      // What a step in hand last reported: the status of a run in progress.
      progress: steps.filter(step => ['claimed', 'working', 'running'].includes(step.state)).map(step => step.last_progress?.split(/\s+/).filter(Boolean).join(' ')).find(Boolean),
    };
  });
  // A failure ages out of the list after its day, as in stui (Nathan, 2026-10-02); showing the
  // hidden missions brings it back.
  const hide = (row: MissionRow) => row.system || failedBeforeToday(row, now);
  const rows = all.filter(row => showSystem || !hide(row)).sort((a, b) => WORDS.indexOf(a.word) - WORDS.indexOf(b.word) || a.title.toLowerCase().localeCompare(b.title.toLowerCase()));
  return { rows, hidden: showSystem ? 0 : all.filter(hide).length };
}

/** A failed mission whose last change was before today, on this device's clock. */
export function failedBeforeToday(row: Pick<MissionRow, 'word' | 'mission'>, now = Date.now()): boolean {
  if (row.word !== 'failed') return false;
  const at = Date.parse(row.mission.updated_at);
  if (Number.isNaN(at)) return false;
  const today = new Date(now); today.setHours(0, 0, 0, 0);
  return at < today.getTime();
}

export type MissionSection = { word: Word; title: string; count: number; rows: MissionRow[] };
export function missionSections(rows: MissionRow[]): MissionSection[] {
  const sections: MissionSection[] = [];
  for (const row of rows) {
    const last = sections.at(-1);
    if (last?.word === row.word) { last.rows.push(row); last.count++; }
    else sections.push({ word: row.word, title: wordName(row.word), count: 1, rows: [row] });
  }
  return sections;
}

type StepStyle = { glyph: string; color: string; word: string; rank: number };
const stepStyles: Record<WorkState, StepStyle> = {
  completed: { glyph: '▰', color: theme.done, word: 'done', rank: 0 },
  cancelled: { glyph: '⊘', color: theme.quiet, word: 'cancelled', rank: 0 },
  failed: { glyph: '✕', color: theme.fault, word: 'failed', rank: 1 },
  claimed: { glyph: '⠿', color: theme.working, word: 'working', rank: 2 },
  verifying: { glyph: '⠿', color: theme.working, word: 'working', rank: 2 },
  ready: { glyph: '▱', color: theme.waiting, word: 'ready', rank: 3 },
  'waiting-person': { glyph: '◐', color: theme.sapphire, word: 'waiting for a person', rank: 4 },
  waiting: { glyph: '◐', color: theme.sapphire, word: 'waiting', rank: 4 },
  blocked: { glyph: '◐', color: theme.sapphire, word: 'waiting', rank: 4 },
};

/** A step's state as stui draws it in a mission's pipeline, done first. */
export function stepStyle(state: string, spinner = '⠿'): StepStyle {
  const style = stepStyles[stepState(state) as WorkState];
  if (!style) return { glyph: '▱', color: theme.surface2, word: 'later', rank: 5 };
  return activeStep(stepState(state)) ? { ...style, glyph: spinner } : style;
}

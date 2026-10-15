import type { Mission, MissionStepPresentation, PresentationActor } from '../../clients/typescript/st3-client';
import { ago } from './presentation';
import { missionSteps } from './presentation';
import { theme } from './theme';

// The Missions list, drawn the way stui draws it (crates/stui/src/ui/screens.rs missions_list):
// one word per mission naming who has to move, in fixtures/clients/words.json order. st serves the
// word, each step and who must act on the mission row (Mission.presentation,
// docs/clients/ui-contract.md, Mission); the phone keeps no derivation of its own.
export const WORDS = ['decision', 'stalled', 'unstaffed', 'unclaimed', 'queued', 'working', 'watching', 'held', 'idle', 'done', 'failed', 'cancelled', 'not_started', 'unknown'] as const;
export type Word = typeof WORDS[number];

export function wordName(word: Word): string {
  return word === 'decision' ? 'needs you' : word === 'not_started' ? 'not started' : word;
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
    case 'not_started': return { glyph: '○', color: theme.quiet };
    case 'unknown': return { glyph: '?', color: theme.quiet };
  }
}

export const MISSION_LEGEND: ReadonlyArray<Word> = ['decision', 'stalled', 'unstaffed', 'queued', 'watching', 'working', 'idle', 'done'];

/** The word st serves for a mission. A daemon too old to serve one, or a word this phone does not
 *  know, reads unknown rather than a guess. */
export function missionWord(mission: Pick<Mission, 'presentation'>): Word {
  const word: string | undefined = mission.presentation?.word;
  return word !== undefined && (WORDS as ReadonlyArray<string>).includes(word) ? word as Word : 'unknown';
}

/** What a list calls a mission: its word, or, when st serves none (an older st, or one that has
 *  not read its agents yet), its raw state and that st does not say. */
export function missionLabel(mission: Pick<Mission, 'presentation' | 'state'>): string {
  const word = missionWord(mission);
  return word === 'unknown' && mission.state ? `${mission.state} · st does not say` : wordName(word);
}

/** Who st says holds a step or must act: "you" for the reader. */
export function actorName(actor: PresentationActor | undefined, reader?: string): string | undefined {
  switch (actor?.kind) {
    case 'person': return actor.person === reader ? 'you' : actor.label ?? actor.person;
    case 'agent': return actor.label ?? actor.agent;
    case 'st': return 'st';
    default: return undefined;
  }
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


export type MissionRow = { mission: Mission; word: Word; label: string; title: string; path: string; age: string; done: number; total: number; system: boolean; progress?: string };
export function missionRows(missions: Mission[], showSystem: boolean, now = Date.now()): { rows: MissionRow[]; hidden: number } {
  const all = missions.map((mission): MissionRow => {
    const progress = mission.presentation?.progress;
    return {
      mission,
      word: missionWord(mission),
      label: missionLabel(mission),
      title: missionTitle(mission),
      path: mission.id.replace(/^mission\//, ''),
      age: ago(mission.updated_at, now),
      done: progress?.done ?? 0,
      total: progress?.total ?? missionSteps(mission).length,
      system: mission.system === true,
      // What a step in hand last reported: the status of a run in progress.
      progress: mission.presentation?.last_progress?.split(/\s+/).filter(Boolean).join(' ') || undefined,
    };
  });
  // A failure ages out of the list after its day, as in stui (Nathan, 2026-10-02); showing the
  // hidden missions brings it back.
  const hide = (row: MissionRow) => row.system || failedBeforeToday(row, now);
  const rows = all.filter(row => showSystem || !hide(row)).sort((a, b) => WORDS.indexOf(a.word) - WORDS.indexOf(b.word) || a.title.toLowerCase().localeCompare(b.title.toLowerCase()));
  return { rows, hidden: showSystem ? 0 : all.filter(hide).length };
}

/** A failed mission that settled before today, on this device's clock. */
export function failedBeforeToday(row: Pick<MissionRow, 'word' | 'mission'>, now = Date.now()): boolean {
  if (row.word !== 'failed') return false;
  const at = Date.parse(row.mission.presentation?.settled_at ?? row.mission.updated_at);
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
    else sections.push({ word: row.word, title: row.word === 'unknown' ? 'st does not say' : wordName(row.word), count: 1, rows: [row] });
  }
  return sections;
}

type StepStyle = { glyph: string; color: string; word: string; rank: number };
const stepStyles: Record<MissionStepPresentation['word'], StepStyle> = {
  done: { glyph: '▰', color: theme.done, word: 'done', rank: 0 },
  cancelled: { glyph: '⊘', color: theme.quiet, word: 'cancelled', rank: 0 },
  needs_you: { glyph: '◆', color: theme.person, word: 'needs you', rank: 1 },
  failed: { glyph: '✕', color: theme.fault, word: 'failed', rank: 1 },
  working: { glyph: '⠿', color: theme.working, word: 'working', rank: 2 },
  ready: { glyph: '▱', color: theme.waiting, word: 'ready', rank: 3 },
  waiting: { glyph: '◐', color: theme.sapphire, word: 'waiting', rank: 4 },
  pending: { glyph: '▱', color: theme.surface2, word: 'later', rank: 5 },
};

/** A step's word as st serves it, drawn as stui draws it in a mission's pipeline. A step st
 *  serves no word for reads unknown. */
export function stepStyle(word: string | undefined, spinner = '⠿'): StepStyle {
  const style = stepStyles[word as MissionStepPresentation['word']];
  if (!style) return { glyph: '?', color: theme.surface2, word: 'unknown', rank: 6 };
  return word === 'working' ? { ...style, glyph: spinner } : style;
}

/** Match every search word against a mission's title, path, status and progress. */
export function filterMissionRows(rows: MissionRow[], filter: string): MissionRow[] {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  return words.length ? rows.filter(row => {
    const text = `${row.title} ${row.path} ${row.label} ${row.progress ?? ''}`.toLowerCase();
    return words.every(word => text.includes(word));
  }) : rows;
}

/**
 * Redesign milestone 1 (RM1): locked decisions and open questions.
 *
 * Locked rows are picks this milestone composes; they are not re-asked.
 * Question IDs are stable: a later revision may add IDs but never reuses or renumbers one.
 * Every open option is a story arg, so each answer has its own deep link.
 */

export interface LockedDecision { readonly id: string; readonly summary: string }
export interface OpenQuestion<K extends string = string> {
  readonly id: string
  readonly arg: K
  readonly question: string
  /** The first option is the milestone default; the others are labelled alternatives. */
  readonly options: readonly { readonly value: string; readonly label: string }[]
}

export const lockedDecisions: readonly LockedDecision[] = [
  { id: 'SR-2', summary: 'Sidebar agent row: 48px two-line row. Line 1: status, title, last turn. Line 2: status word (when not working), host, spend, duration.' },
  { id: 'WF-1', summary: 'Work log: routine tool calls fold into one "Worked for" row; edits and failures stay visible.' },
  { id: 'RC-1', summary: 'Resource card: 40px flat wash card that opens the resource in a split. Composed here as the subagent result card in the parent answer.' },
  { id: 'RC-3', summary: 'File chips inside prose only (baseline-aligned, hover preview).' },
  { id: 'SG-1', summary: 'Status glyph: 8px disc in a fixed 16px column; no extra leading pip or dot.' },
  { id: 'RM1-Q1', summary: 'Rows stay two lines at every sidebar width (no one-line fold).' },
  { id: 'RM1-Q2', summary: 'Roster loading shows row-shaped placeholders at the real 48px row pitch.' },
  { id: 'RM1-Q3', summary: 'A roster failure appears inside the sidebar, where the list would be.' },
  { id: 'RM1-Q4', summary: 'With no agent open, the workspace shows one line plus keyboard hints.' },
  { id: 'Composer', summary: 'Not part of this milestone. The composer is the current one, visually unchanged, until its scope is revisited.' },
]

export type QuestionArg = 'paletteScope' | 'glyphRefinement' | 'sectionNav'

export const openQuestions: readonly OpenQuestion<QuestionArg>[] = [
  { id: 'RM1-Q5', arg: 'paletteScope', question: 'What does Mod+K search by default?', options: [
    { value: 'mixed', label: 'Agents first, then commands' },
    { value: 'agents', label: 'Agents only; type ">" for commands' },
  ] },
  { id: 'RM1-Q6', arg: 'glyphRefinement', question: 'Which SG-1 refinement tells states apart without colour?', options: [
    { value: 'G3', label: 'States other than working and idle add a shape (needs you ?, stale clock, ended square)' },
    { value: 'G1', label: 'Plain discs' },
    { value: 'G2', label: 'Running disc gains a turning ring' },
  ] },
  { id: 'RM1-Q7', arg: 'sectionNav', question: 'Where do Inbox, Missions and Resources sit in the sidebar?', options: [
    { value: 'footer', label: 'One compact row under the agent list' },
    { value: 'header', label: 'One compact row above the agent list' },
  ] },
]

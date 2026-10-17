import { createHash } from 'node:crypto'

// The per-commit play set (KIT_PLAY_SET=commit). Pull requests run it; merge groups and manual runs
// run every story in all four cases with axe (KIT_PLAY_SET=all), so nothing here leaves the merge gate.
//
// It starts from the stories tagged `critical` and narrows them in two ways, which is what makes it
// fit the per-commit budget (about 60 s on the 4-vCPU runner; the critical set took about 185 s):
//
//  1. Each story runs in one case, not four. The case is one of two diagonals of the
//     scheme × width matrix, chosen by a stable hash of the story id, so about half the stories run
//     dark at 440 and half light at 1440: every story's play runs, and both schemes and both widths
//     stay covered across the set. Stories in BOTH_WIDTHS are about layout at a width, so they run
//     dark at 440 and at 1440.
//  2. Stories in MERGE_ONLY do not run per commit. They are the slowest stories whose coverage is
//     behavioural depth or render cost rather than a kit contract, and the merge gate still runs them.
//
// Every id named here must be a critical story, so a rename cannot silently drop coverage.
export const MERGE_ONLY = [
  // 15 s of case time: every state of the transcript's no-green rule in one story.
  'fractal-ui-transcript--no-green-all-states',
  // 10 s of case time: renders a hundred agents; a render-cost check rather than a contract.
  'fractal-ui-sidebar-roster--hundred-agent-render',
]
export const BOTH_WIDTHS = [
  'fractal-ui-composer--footer-width-budget',
  'fractal-ui-composer--kit-label-resize',
  'fractal-ui-composer--layout-shapes',
  'fractal-ui-sidebar-agent-row--densities-and-children',
  'fractal-ui-transcript-composer--host-width',
  'fractal-ui-transcript-composer--reading-column',
  'fractal-ui-workbench--all-states',
]

const FULL = [['dark', 440], ['dark', 1440], ['light', 440], ['light', 1440]]

// A stable pick of one corner pair, independent of the other stories in the set.
export const diagonalCase = id => FULL[createHash('sha256').update(id).digest()[0] % 2 === 0 ? 0 : 3]

// The cases of the per-commit set for the critical story ids, plus what the set leaves out.
export function commitSetCases(criticalIds, { mergeOnlyIds = MERGE_ONLY, bothWidthsIds = BOTH_WIDTHS } = {}) {
  const critical = new Set(criticalIds)
  for (const id of [...mergeOnlyIds, ...bothWidthsIds]) {
    if (!critical.has(id)) throw new Error(`The per-commit set names ${id}, which is not a critical story`)
  }
  const mergeOnly = new Set(mergeOnlyIds), bothWidths = new Set(bothWidthsIds)
  const ids = criticalIds.filter(id => !mergeOnly.has(id))
  const cases = ids.flatMap(id => (bothWidths.has(id) ? [FULL[0], FULL[1]] : [diagonalCase(id)]).map(([scheme, width]) => ({ id, scheme, width })))
  return { ids, cases, omittedStories: [...mergeOnlyIds], omittedCases: criticalIds.length * 4 - cases.length }
}

export const fullCases = ids => ids.flatMap(id => FULL.map(([scheme, width]) => ({ id, scheme, width })))

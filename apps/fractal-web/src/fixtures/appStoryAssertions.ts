import { appStoryStates } from './storybookScenarios.ts'

/** Used by both CSF plays and the planted stub control. A harness label is never
 * evidence: every state must contain the real pane's transcript and composer. */
export const assertProductionAppStates = (canvas: HTMLElement, workspace: boolean): void => {
  for (const state of appStoryStates) {
    const specimen = canvas.querySelector(`[data-app-state="${state}"]`)
    const transcript = state === 'unavailable' ? 'transcript-unavailable' : 'transcript-scroll'
    if (specimen === null || specimen.querySelector(`[data-testid="${transcript}"]`) === null)
      throw new Error(`Production transcript missing in ${state}`)
    if (specimen.querySelector('[data-testid="conversation-composer-dock"]') === null)
      throw new Error(`Production composer missing in ${state}`)
    if (workspace && specimen.querySelector('[data-testid="live-agent-workspace"]') === null)
      throw new Error(`Production workspace missing in ${state}`)
    if (state === 'empty' && specimen.querySelector('[data-testid="transcript-empty"]') === null)
      throw new Error('Production empty transcript missing')
    if (state === 'loading' && specimen.querySelector('[data-testid="transcript-placeholder"]') === null)
      throw new Error('Production loading transcript missing')
    if (['populated', 'failed-tool', 'offline', 'scripted-repair'].includes(state) && specimen.querySelector('[data-testid="transcript-turn"]') === null)
      throw new Error(`Production populated transcript missing in ${state}`)
    if (state === 'unavailable' && specimen.querySelector('[data-wf-unavailable]') === null)
      throw new Error('Production unavailable transcript missing')
  }
}

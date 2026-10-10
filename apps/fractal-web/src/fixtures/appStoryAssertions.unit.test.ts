// @vitest-environment jsdom
import { expect, it } from 'vitest'
import { assertProductionAppStates } from './appStoryAssertions.ts'
import { appStoryStates } from './storybookScenarios.ts'

it('negative control: the exact play assertion rejects a production component replaced by a stub', () => {
  const canvas = document.createElement('div')
  // Keep all story labels: matching the harness alone must not pass a play.
  canvas.innerHTML = appStoryStates.map(state => `<section data-app-state="${state}"><div>Stubbed production module</div></section>`).join('')
  expect(() => assertProductionAppStates(canvas, false)).toThrow('Production transcript missing in loading')
  expect(() => assertProductionAppStates(canvas, true)).toThrow('Production transcript missing in loading')
  console.info('NEGATIVE CONTROL: production stub rejected: Production transcript missing in loading (pane and workspace plays)')
})

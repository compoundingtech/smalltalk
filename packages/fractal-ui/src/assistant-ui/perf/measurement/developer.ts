// From compoundingtech/smalltalk#1641, commit 4f74464e0b22ea889a09d9271d076c3c6fd85df8.
import { incrDebug, counters } from './counters.ts'
import { createMeasurementEngine } from './engine.ts'

/** React Profiler onRender adapter: one callback is one subtree commit, not one body render. */
export const recordCommit = (id: string): void => { incrDebug(`Renders.${id}`) }

/** Text-only workshop HUD, independent of production app styling and counters. */
export const mountMeasurementHud = (parent: HTMLElement, framePeriodMs = 1000 / 60) => {
  const engine = createMeasurementEngine({ framePeriodMs })
  const output = document.createElement('pre')
  output.setAttribute('aria-label', 'Fractal measurement HUD')
  parent.append(output)
  const refresh = setInterval(() => { output.textContent = JSON.stringify(counters.snapshot(), undefined, 2) }, 250)
  return {
    engine,
    dispose: () => { clearInterval(refresh); output.remove(); engine.dispose() },
  }
}

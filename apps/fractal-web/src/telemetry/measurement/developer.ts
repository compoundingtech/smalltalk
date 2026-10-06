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

import * as React from 'react'

import { counters, developmentMeasurements, getDebug, incrDebug, incrDebugRuntime, setDebug } from './measurement/index.ts'
import type { DebugBag } from './measurement/index.ts'

export { getDebug, incrDebug, incrDebugRuntime, setDebug }
export type { DebugBag }

type SharedMeasurementEngine = ReturnType<NonNullable<typeof developmentMeasurements>['createMeasurementEngine']>
let sharedMeasurementEngine: SharedMeasurementEngine | undefined
let measurementEngineUsers = 0

/** Acquire the app's single public engine and publish it for browser measurement clients. */
export const acquireMeasurementEngine = (): (() => void) | undefined => {
  if (developmentMeasurements === undefined) return undefined
  if (sharedMeasurementEngine === undefined) {
    sharedMeasurementEngine = developmentMeasurements.createMeasurementEngine()
    const global = globalThis as typeof globalThis & { __metersEngine?: SharedMeasurementEngine }
    global.__metersEngine = sharedMeasurementEngine
  }
  const engine = sharedMeasurementEngine
  measurementEngineUsers += 1
  let released = false
  return () => {
    if (released) return
    released = true
    measurementEngineUsers -= 1
    if (measurementEngineUsers === 0 && sharedMeasurementEngine === engine) {
      const global = globalThis as typeof globalThis & { __metersEngine?: SharedMeasurementEngine }
      if (global.__metersEngine === engine) delete global.__metersEngine
      engine.dispose()
      sharedMeasurementEngine = undefined
    }
  }
}

/** One Profiler callback corresponds to one committed subtree update. */
export const RenderProfiler = ({ id, children }: { readonly id: string; readonly children: React.ReactNode }) => (
  <React.Profiler id={id} onRender={(profiledId) => developmentMeasurements?.recordCommit(profiledId)}>
    {children}
  </React.Profiler>
)

interface VitalBlockBase {
  readonly key: string
  readonly width: number
  readonly height: number
}
interface FpsBlock extends VitalBlockBase { readonly kind: 'fps' }
interface RenderBlock extends VitalBlockBase { readonly kind: 'renders'; readonly ids: readonly string[] }
interface CounterBlock extends VitalBlockBase {
  readonly kind: 'counter'
  readonly getValue: () => number
  readonly getText: (value: number) => string
  readonly thresholds?: { readonly yellow: number; readonly red: number }
}
interface ValueBlock extends VitalBlockBase {
  readonly kind: 'value'
  readonly getFrameValue: () => number
  readonly getText: (value: number) => string
}
type VitalBlock = FpsBlock | RenderBlock | CounterBlock | ValueBlock

let nextBlockId = 0
const blockId = (): string => `measurement-${++nextBlockId}`

export const makeFpsMeterBlock = ({ height, width }: { readonly height: number; readonly width: number }): VitalBlock => ({
  kind: 'fps', key: blockId(), height, width,
})

export const makeRenderProfilerBlock = ({
  ids, height, width,
}: {
  readonly ids: readonly string[]
  readonly height: number
  readonly width: number
}): VitalBlock => ({ kind: 'renders', key: blockId(), ids, height, width })

export const makeCounterBlock = ({
  getValue, getText, height, width, thresholds,
}: {
  readonly getValue: () => number
  readonly getText: (value: number) => string
  readonly height: number
  readonly width: number
  readonly thresholds?: { readonly yellow: number; readonly red: number }
}): VitalBlock => ({ kind: 'counter', key: blockId(), getValue, getText, height, width, thresholds })

export const makeValueMeterBlock = ({
  getFrameValue, getText, height, width,
}: {
  readonly getFrameValue: () => number
  readonly getText: (value: number) => string
  readonly height: number
  readonly width: number
  readonly textMode?: 'latest'
}): VitalBlock => ({ kind: 'value', key: blockId(), getFrameValue, getText, height, width })

/** Simple text diagnostics: values come from the app's measurement counters and rAF clock. */
export const AppVitals = ({
  blocks, gap = 4, orientation = 'horizontal', exposeGlobally = false,
}: {
  readonly blocks: readonly VitalBlock[]
  readonly gap?: number
  readonly orientation?: 'horizontal' | 'vertical'
  readonly exposeGlobally?: boolean
}) => {
  React.useEffect(() => {
    if (!exposeGlobally) return undefined
    return acquireMeasurementEngine()
  }, [exposeGlobally])

  return (
    <div
      aria-label="Application measurements"
      style={{ display: 'flex', flexDirection: orientation === 'horizontal' ? 'row' : 'column', gap }}
    >
      {blocks.map((block) => <BlockView key={block.key} block={block} />)}
    </div>
  )
}

const BlockView = ({ block }: { readonly block: VitalBlock }) => {
  const [fps, setFps] = React.useState<number | undefined>(undefined)
  const [, refresh] = React.useReducer((value: number) => value + 1, 0)
  React.useEffect(() => {
    if (block.kind === 'fps') {
      let frameCount = 0
      let intervalStarted: number | undefined
      let frame = 0
      const sample = (time: number) => {
        frameCount += 1
        intervalStarted ??= time
        const elapsed = time - intervalStarted
        if (elapsed >= 500) {
          setFps(frameCount * 1000 / elapsed)
          frameCount = 0
          intervalStarted = time
        }
        frame = requestAnimationFrame(sample)
      }
      frame = requestAnimationFrame(sample)
      return () => cancelAnimationFrame(frame)
    }
    const timer = window.setInterval(() => refresh(), 250)
    return () => window.clearInterval(timer)
  }, [block.kind])

  const style: React.CSSProperties = {
    boxSizing: 'border-box', minHeight: block.height, width: block.width,
    display: 'flex', alignItems: 'center', paddingInline: 5, overflow: 'hidden',
    whiteSpace: 'nowrap', font: '10px ui-monospace, monospace', fontVariantNumeric: 'tabular-nums',
  }
  if (block.kind === 'fps') return <span style={style}>FPS {fps === undefined ? '—' : fps.toFixed(1)}</span>
  if (block.kind === 'renders') {
    const text = block.ids.map((id) => `${id} ${counters.getDebug(`Renders.${id}`)}`).join(' · ')
    return <span style={style} title={text}>{text || 'Renders —'}</span>
  }
  const value = block.kind === 'counter' ? block.getValue() : block.getFrameValue()
  const severity = block.kind === 'counter' && block.thresholds !== undefined
    ? value >= block.thresholds.red ? 'red' : value >= block.thresholds.yellow ? 'yellow' : undefined
    : undefined
  return <span style={{ ...style, color: severity === 'red' ? '#f87171' : severity === 'yellow' ? '#facc15' : undefined }}>{block.getText(value)}</span>
}

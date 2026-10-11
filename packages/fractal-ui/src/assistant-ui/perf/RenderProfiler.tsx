import * as React from 'react'
import { developmentMeasurements } from './measurement'

const recordCommit: React.ProfilerOnRenderCallback = id => developmentMeasurements?.recordCommit(id)

/** Public meter adapter; counts subtree commits, not component function calls. */
export function RenderProfiler({ id, children }: { readonly id: string; readonly children: React.ReactNode }) {
  return developmentMeasurements === undefined
    ? children
    : <React.Profiler id={id} onRender={recordCommit}>{children}</React.Profiler>
}

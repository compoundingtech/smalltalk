import { live, telemetry } from './liveRuntime.ts'
import * as React from 'react'
import { DataSourceProvider } from '../data/react.tsx'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

// A stored preference must never pull diagnostic modules into a production build.
const Diagnostics = import.meta.env.DEV || import.meta.env.PERF
  ? React.lazy(() => import('../telemetry/DevBar.tsx').then(({ DevBar }) => ({ default: DevBar })))
  : undefined

export const App = () => (
  <>
    {Diagnostics !== undefined && (
      <React.Suspense fallback={null}>
        <Diagnostics defaultVisible />
      </React.Suspense>
    )}
    <DataSourceProvider source={live.source} registry={live.registry}>
      <React.Profiler id="workbench" onRender={telemetry.onCommit}>
        <LiveAgentWorkspace ux={telemetry.ux} onSelectConversation={live.selectConversation} />
      </React.Profiler>
    </DataSourceProvider>
  </>
)

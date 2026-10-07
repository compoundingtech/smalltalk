import { live, telemetry } from './liveRuntime.ts'
import * as React from 'react'
import { DataSourceProvider } from '../data/react.tsx'
import { DevBar } from '../telemetry/DevBar.tsx'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'

export const App = () => (
  <>
    <DevBar />
    <DataSourceProvider source={live.source} registry={live.registry}>
      {telemetry === undefined ? (
        <LiveAgentWorkspace />
      ) : (
        <React.Profiler id="workbench" onRender={telemetry.onCommit}>
          <LiveAgentWorkspace />
        </React.Profiler>
      )}
    </DataSourceProvider>
  </>
)

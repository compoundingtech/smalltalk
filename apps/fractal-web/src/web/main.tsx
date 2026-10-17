import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { setTheme } from '../ui-compat/foundations.ts'

import { App } from './App.tsx'
import { live } from './liveRuntime.ts'
import { installPageLifecycle } from './pageLifecycle.ts'
import { markStart } from './perfMarks.ts'

markStart('sidebar_interactive', { startTime: 0 })
const initialAgent = window.location.pathname.match(/^\/w\/(.+)$/)?.[1]
if (initialAgent !== undefined) {
  try { markStart('open_transcript', { startTime: 0, key: decodeURIComponent(initialAgent) }) } catch { /* Invalid routes have no transcript to measure. */ }
}

setTheme('system')

const rootElement = document.getElementById('root')
if (rootElement === null) {
  throw new Error('Root element not found')
}

const root = createRoot(rootElement)
let disposed = false
installPageLifecycle({
  target: window,
  source: live,
  beforeDispose: () => {
    disposed = true
    root.unmount()
  },
})

void live.ready.then(() => {
  if (disposed) return
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
})

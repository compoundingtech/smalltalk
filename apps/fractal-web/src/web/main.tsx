import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { setTheme } from '../ui-compat/foundations.ts'

import { App } from './App.tsx'
import { live } from './liveRuntime.ts'
import { installPageLifecycle } from './pageLifecycle.ts'

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

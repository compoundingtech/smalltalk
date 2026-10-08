import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { setTheme } from '../ui-compat/foundations.ts'

import { App } from './App.tsx'
import { live } from './liveRuntime.ts'

setTheme('system')

const rootElement = document.getElementById('root')
if (rootElement === null) {
  throw new Error('Root element not found')
}

const root = createRoot(rootElement)
let disposed = false
window.addEventListener('pagehide', (event) => {
  if (event.persisted) return
  disposed = true
  // Stop registry consumers in the same event before asynchronous source teardown can rerender them.
  root.unmount()
})

void live.ready.then(() => {
  if (disposed) return
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
})

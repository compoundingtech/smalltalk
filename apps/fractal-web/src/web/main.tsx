// A module import, not a document link: Vite resolves its @import into the entry stylesheet,
// which `deferBuiltStylesheets` keeps from blocking the themed shell. First, so it precedes component rules.
import './app.css'

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { setTheme } from '../ui-compat/foundations.ts'

import { App } from './App.tsx'
import { live } from './liveRuntime.ts'
import { installPageLifecycle } from './pageLifecycle.ts'

declare global {
  interface Window {
    __wfStylexReady?: Promise<void>
  }
}

setTheme(document.documentElement.dataset.scheme === 'light' ? 'light' : 'dark')

const rootElement = document.getElementById('root')
if (rootElement === null) {
  throw new Error('Root element not found')
}

const root = createRoot(rootElement)
// Production: every deferred bundle stylesheet. Development: the StyleX runtime sheet; the
// runtime replaces its link with an inline stylesheet, so link.sheet is no readiness signal.
const stylesReady = window.__wfStylexReady
let disposed = false
installPageLifecycle({
  target: window,
  source: live,
  beforeDispose: () => {
    disposed = true
    root.unmount()
  },
})

void Promise.all([live.ready, stylesReady]).then(() => {
  if (disposed) return
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
})

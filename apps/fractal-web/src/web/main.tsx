import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { setTheme } from '../ui-compat/foundations.ts'

import { App } from './App.tsx'

setTheme('system')

const rootElement = document.getElementById('root')
if (rootElement === null) {
  throw new Error('Root element not found')
}

createRoot(rootElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
)

import * as React from 'react'
import { statusVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import type { Agent } from '../data/source.ts'

export type FaviconState = 'working' | 'needs-you' | 'error' | 'none'
// Pending a kit status token: the kit's running token is blue, not green.
export const faviconWorkingColor = '#22c55e'

export const tabTitle = ({ agentName, needsYouCount, offline }: {
  readonly agentName?: string
  readonly needsYouCount: number
  readonly offline: boolean
}): string => agentName === undefined ? 'Fractal' : offline ? `Offline · ${agentName} · Fractal`
  : `${needsYouCount === 0 ? '' : `(${needsYouCount}) `}${agentName} · Fractal`

/** Native waiting does not mean needs-you; only the shared attention projection does. */
export const selectedFaviconState = ({ agent, needsYou, offline }: {
  readonly agent?: Agent
  readonly needsYou: boolean
  readonly offline: boolean
}): FaviconState => {
  if (agent === undefined || offline || !agent.connected ||
    !['running', 'working', 'waiting', 'idle', 'done', 'failed', 'errored'].includes(agent.state ?? '')) return 'none'
  if (agent.activity === 'errored') return 'error'
  if (needsYou) return 'needs-you'
  if (agent.state === 'done' || agent.state === 'idle') return 'none'
  return agent.activity === 'working' ? 'working' : 'none'
}

/** Resolve StyleX theme variables in the selected workspace, not a duplicated palette. */
export const faviconDotColor = ({ state, themeRoot }: {
  readonly state: FaviconState
  readonly themeRoot: HTMLElement
}): string | undefined => {
  if (state === 'none') return undefined
  if (state === 'working') return faviconWorkingColor
  const probe = document.createElement('span')
  probe.style.color = state === 'needs-you' ? statusVars.attention : statusVars.danger
  probe.hidden = true
  themeRoot.append(probe)
  const color = getComputedStyle(probe).color
  probe.remove()
  return color
}

const drawFavicon = (color: string | undefined): string => {
  const canvas = document.createElement('canvas')
  canvas.width = canvas.height = 32
  const context = canvas.getContext('2d')!
  // The same 32px Fractal mark as index.html; the badge does not replace the icon.
  context.fillStyle = '#18181b'
  context.beginPath()
  context.roundRect(0, 0, 32, 32, 7)
  context.fill()
  context.fillStyle = '#fafafa'
  context.fillRect(9, 8, 15, 4)
  context.fillRect(9, 12, 4, 14)
  context.fillRect(13, 16, 9, 4)
  if (color !== undefined) {
    context.beginPath()
    context.arc(26, 26, 5, 0, 2 * Math.PI)
    context.fillStyle = color
    context.fill()
    context.strokeStyle = '#18181b'
    context.lineWidth = 2
    context.stroke()
  }
  return canvas.toDataURL('image/png')
}

/** DOM-only commit effects: no React state, subscriptions, polling or additional renders. */
export const useTabMetadata = ({ title, faviconState }: {
  readonly title: string
  readonly faviconState: FaviconState
}): void => {
  const lastFavicon = React.useRef<FaviconState | undefined>(undefined)
  React.useLayoutEffect(() => {
    if (document.title !== title) document.title = title
  }, [title])
  React.useLayoutEffect(() => {
    if (lastFavicon.current === faviconState) return
    const icon = document.querySelector<HTMLLinkElement>('link[rel="icon"]')
    if (icon === null) return
    const themeRoot = document.querySelector<HTMLElement>('[data-testid="live-agent-workspace"]') ?? document.documentElement
    icon.type = 'image/png'
    icon.href = drawFavicon(faviconDotColor({ state: faviconState, themeRoot }))
    lastFavicon.current = faviconState
  }, [faviconState])
}

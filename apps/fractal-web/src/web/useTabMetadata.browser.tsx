import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import type { Agent } from '../data/source.ts'
import { selectedFaviconState, useTabMetadata } from './useTabMetadata.ts'

// Browser fixture, served by the existing Vite server. No canvas or StyleX mocks.
// ?negative=working swaps the working agent for an error agent without changing its pixel oracle.
const agent: Agent = { ref: 'agent/example', terminal: '', name: 'Example', lifecycle: { _tag: 'Unknown' }, host: 'host/example', connected: true, activity: 'working', status: 'working', state: 'running', usage: { _tag: 'Unknown' }, checkout: { _tag: 'Unknown' }, workspace: { _tag: 'Unknown' }, startedAt: { _tag: 'Unknown' }, endedAt: { _tag: 'Unknown' }, blockedOn: { _tag: 'Unknown' }, ask: { _tag: 'Unknown' }, lastActivityAt: { _tag: 'Unknown' } }
const fixtures = [
  { name: 'working', agent, needsYou: false, offline: false, expected: [34, 197, 94, 255] },
  { name: 'error', agent: { ...agent, activity: 'errored' as const, state: 'failed' }, needsYou: false, offline: false, expected: [251, 65, 74, 255] },
  { name: 'needs-you', agent, needsYou: true, offline: false, expected: [254, 154, 0, 255] },
  { name: 'unknown', agent: { ...agent, state: undefined }, needsYou: false, offline: false, expected: [24, 24, 27, 255] },
  { name: 'offline', agent, needsYou: true, offline: true, expected: [24, 24, 27, 255] },
]
const Metadata = ({ fixture }: { fixture: typeof fixtures[number] }) => {
  useTabMetadata({ title: `${fixture.name} · Example`, faviconState: selectedFaviconState(fixture) })
  return null
}
const root = createRoot(document.getElementById('root')!)
const failures: string[] = []
const pixels: Record<string, readonly number[]> = {}
try {
  for (const fixture of fixtures) {
    const rendered = new URLSearchParams(location.search).get('negative') === fixture.name
      ? { ...fixture, agent: { ...agent, activity: 'errored' as const, state: 'failed' } }
      : fixture
    flushSync(() => root.render(<Metadata fixture={rendered} />))
    const icon = document.querySelector<HTMLLinkElement>('link[rel="icon"]')!
    const image = new Image()
    image.src = icon.href
    await image.decode()
    const canvas = document.createElement('canvas')
    canvas.width = canvas.height = 32
    const context = canvas.getContext('2d')!
    context.drawImage(image, 0, 0)
    const actual = Array.from(context.getImageData(26, 26, 1, 1).data)
    pixels[fixture.name] = actual
    if (JSON.stringify(actual) !== JSON.stringify(fixture.expected))
      failures.push(`${fixture.name}: expected ${fixture.expected}, decoded ${actual}`)
    // Keep decoded images available for exact 32px screenshot crops.
    image.id = `favicon-${fixture.name}`
    image.alt = `${fixture.name} favicon`
    document.body.append(image)
  }
  document.body.dataset.proof = JSON.stringify({ pixels, failures })
  document.body.dataset.testResult = failures.length === 0 ? 'pass' : 'fail'
} catch (error) {
  document.body.dataset.proof = String(error instanceof Error ? error.stack : error)
  document.body.dataset.testResult = 'fail'
} finally {
  root.unmount()
}

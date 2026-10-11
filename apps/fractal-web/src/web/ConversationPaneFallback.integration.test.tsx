// @vitest-environment jsdom
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { EmbraceRuntimeProvider, Transcript } from '@smalltalk/fractal-ui/assistant-ui'
import { ConversationPaneFallback } from './ConversationPaneFallback.tsx'

// Keep the real kit/runtime DOM. Expose StyleX's precompiled declarations as test metadata
// so a changed header, lane or skeleton dimension is caught along with structural drift.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: (...styles: unknown[]) => ({
    'data-test-layout': JSON.stringify(Object.fromEntries(styles.flatMap(style =>
      typeof style === 'object' && style !== null ? Object.entries(style) : [],
    ))),
  }),
}))

interface RenderedShape {
  readonly tag: string
  readonly attributes: Readonly<Record<string, string>>
  readonly layout: unknown
  readonly text: string
  readonly children: readonly RenderedShape[]
}
const shape = (element: Element): RenderedShape => ({
  tag: element.tagName,
  attributes: Object.fromEntries([...element.attributes]
    .filter(attribute => ['data-testid', 'data-sync-state', 'data-sync-visible', 'aria-label', 'aria-hidden', 'aria-live', 'aria-busy', 'role', 'tabindex', 'hidden', 'disabled'].includes(attribute.name))
    .map(attribute => [attribute.name, attribute.value])),
  layout: JSON.parse(element.getAttribute('data-test-layout') ?? '{}'),
  text: [...element.childNodes].filter(node => node.nodeType === Node.TEXT_NODE).map(node => node.textContent).join(''),
  children: [...element.children].map(shape),
})

const transcriptRoot = (element: React.ReactElement): Element => {
  const container = document.createElement('div')
  container.innerHTML = renderToStaticMarkup(element)
  const root = container.querySelector('[aria-label="Transcript"]')
  if (root === null) throw new Error('Missing transcript frame')
  return root
}

describe('conversation code-loading fallback', () => {
  it('matches the real kit Waiting transcript structure, testids, accessible labels and geometry', () => {
    const waiting = transcriptRoot(<EmbraceRuntimeProvider options={{ messages: [], onNew: async () => { throw new Error('A Waiting transcript must not send a message') } }}>
      <Transcript turns={[]} title="Example Agent" sync={{ _tag: 'Requested', since: 0 }} now={0} observedAt={0} />
    </EmbraceRuntimeProvider>)
    const fallback = transcriptRoot(<ConversationPaneFallback agentName="Example Agent" />)

    expect(shape(fallback)).toEqual(shape(waiting))
    expect([...fallback.querySelectorAll('[data-testid]')].map(node => node.getAttribute('data-testid')))
      .toEqual(['transcript-header', 'transcript-scroll', 'transcript-placeholder', 'sync-line'])
    expect(fallback.querySelector('[data-testid="transcript-placeholder"]')?.getAttribute('aria-label')).toBe('Loading conversation')
    expect(fallback.querySelector('[data-testid="transcript-scroll"]')?.getAttribute('aria-label')).toBe('Conversation history')
    expect(fallback.querySelector('header')?.textContent).toBe('Example Agent')
    expect(fallback.querySelector('textarea')).toBeNull()
  })

  it('keeps a still-loading retained hidden pane out of the accessibility tree', () => {
    const container = document.createElement('div')
    container.innerHTML = renderToStaticMarkup(<ConversationPaneFallback agentName="Hidden Agent" visible={false} />)
    expect(container.firstElementChild?.getAttribute('aria-hidden')).toBe('true')
    expect(container.querySelector('[aria-label="Loading conversation"]')).not.toBeNull()
  })
})

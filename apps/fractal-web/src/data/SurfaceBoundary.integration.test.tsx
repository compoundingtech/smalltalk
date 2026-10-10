// @vitest-environment jsdom
/**
 * A renderer defect stays inside its own surface: the failing surface shows a labelled fallback
 * with the reason, its siblings keep rendering, and an unsent draft (an uncontrolled DOM value)
 * survives because React never replaces the sibling's element.
 */
import * as React from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
// Node tests stub only the CSS runtime; the boundary's behaviour is what is under test.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))

import { SurfaceActivity, SurfaceBoundary } from './SurfaceBoundary.tsx'

const FailingSurface = ({ failed }: { readonly failed: boolean }) => {
  if (failed) throw new Error('Resource renderer failed')
  return <p>Resource contents</p>
}

// React reports every caught render error; the defect is the scenario under test, not noise.
beforeEach(() => {
  vi.spyOn(console, 'error').mockImplementation(() => {})
})
afterEach(() => {
  vi.restoreAllMocks()
})

const mount = () => {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const byText = (text: string) =>
    [...container.querySelectorAll('p')].find((element) => element.textContent === text)
  const failure = (label: string) =>
    container.querySelector(`[role="alert"][aria-label="${label} rendering failure"]`)
  const textbox = (label: string) => {
    const element = container.querySelector(`textarea[aria-label="${label}"]`)
    if (!(element instanceof HTMLTextAreaElement)) throw new Error(`no textbox ${label}`)
    return element
  }
  return {
    container,
    byText,
    failure,
    textbox,
    render: (element: React.ReactNode) => flushSync(() => root.render(element)),
    unmount: () => {
      flushSync(() => root.unmount())
      container.remove()
    },
  }
}

it('isolates a renderer defect without losing the transcript or an unsent draft, and recovers for another subject', async () => {
  const view = mount()
  const render = (subject: string, failed: boolean) =>
    view.render(
      <>
        <SurfaceBoundary label="Transcript">
          <p>Retained transcript</p>
        </SurfaceBoundary>
        <SurfaceBoundary label="Composer">
          <textarea aria-label="Unsent message" defaultValue="" />
        </SurfaceBoundary>
        <SurfaceBoundary key={subject} label="Resources">
          <FailingSurface failed={failed} />
        </SurfaceBoundary>
      </>,
    )
  try {
    render('agent/first', false)
    await vi.waitFor(() => expect(view.byText('Resource contents')).toBeDefined())
    const draft = view.textbox('Unsent message')
    draft.value = 'Do not discard this draft'
    render('agent/first', true)
    await vi.waitFor(() => expect(view.failure('Resources')).not.toBeNull())
    expect(view.failure('Resources')!.textContent).toContain('Resource renderer failed')
    expect(view.failure('Resources')!.textContent).toContain(
      'Resources could not be displayed. Other surfaces remain available.',
    )
    expect(view.byText('Resource contents')).toBeUndefined()
    expect(view.byText('Retained transcript')).toBeDefined()
    expect(view.textbox('Unsent message')).toBe(draft)
    expect(draft.value).toBe('Do not discard this draft')
    render('agent/second', false)
    await vi.waitFor(() => expect(view.byText('Resource contents')).toBeDefined())
    expect(view.failure('Resources')).toBeNull()
    expect(view.textbox('Unsent message')).toBe(draft)
    expect(draft.value).toBe('Do not discard this draft')
  } finally {
    view.unmount()
  }
})

it('retries a failed retained surface on Activity reactivation without replacing its sibling draft', async () => {
  const view = mount()
  const render = (mode: 'visible' | 'hidden', failed: boolean) =>
    view.render(
      <SurfaceActivity mode={mode}>
        <SurfaceBoundary label="Resources">
          <FailingSurface failed={failed} />
        </SurfaceBoundary>
        <SurfaceBoundary label="Composer">
          <textarea aria-label="Retained unsent message" defaultValue="" />
        </SurfaceBoundary>
      </SurfaceActivity>,
    )
  try {
    render('visible', false)
    await vi.waitFor(() => expect(view.byText('Resource contents')).toBeDefined())
    const draft = view.textbox('Retained unsent message')
    draft.value = 'Keep the retained draft'
    render('visible', true)
    await vi.waitFor(() => expect(view.failure('Resources')).not.toBeNull())
    // The defect stays latched while the activation is unchanged, even once the renderer heals.
    render('visible', false)
    await vi.waitFor(() => expect(view.failure('Resources')).not.toBeNull())
    expect(view.byText('Resource contents')).toBeUndefined()
    // Activity hides retained children with an inline `display: none`, keeping them mounted.
    render('hidden', false)
    await vi.waitFor(() => expect(draft.style.display).toBe('none'))
    render('visible', false)
    await vi.waitFor(() => expect(view.byText('Resource contents')).toBeDefined())
    expect(view.failure('Resources')).toBeNull()
    expect(draft.style.display).not.toBe('none')
    expect(view.textbox('Retained unsent message')).toBe(draft)
    expect(draft.value).toBe('Keep the retained draft')
  } finally {
    view.unmount()
  }
})

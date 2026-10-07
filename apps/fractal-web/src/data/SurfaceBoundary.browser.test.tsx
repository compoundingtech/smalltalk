import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it } from 'vitest'
import { page, userEvent } from 'vitest/browser'

import { SurfaceActivity, SurfaceBoundary } from './SurfaceBoundary.tsx'

const FailingSurface = ({ failed }: { readonly failed: boolean }) => {
  if (failed) throw new Error('Resource renderer failed')
  return <p>Resource contents</p>
}

it('isolates a renderer defect without losing the transcript or an unsent draft, and recovers for another subject', async () => {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = (subject: string, failed: boolean) =>
    root.render(
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
    await expect.element(page.getByText('Resource contents', { exact: true })).toBeVisible()
    const draft = page.getByRole('textbox', { name: 'Unsent message' })
    await userEvent.fill(draft, 'Do not discard this draft')
    render('agent/first', true)
    await expect
      .element(page.getByRole('alert', { name: 'Resources rendering failure' }))
      .toBeVisible()
    await expect.element(page.getByText('Retained transcript', { exact: true })).toBeVisible()
    await expect.element(draft).toHaveValue('Do not discard this draft')
    render('agent/second', false)
    await expect.element(page.getByText('Resource contents', { exact: true })).toBeVisible()
    await expect
      .element(page.getByRole('alert', { name: 'Resources rendering failure' }))
      .not.toBeInTheDocument()
    await expect.element(draft).toHaveValue('Do not discard this draft')
  } finally {
    root.unmount()
    container.remove()
  }
})

it('retries a failed retained surface on Activity reactivation without replacing its sibling draft', async () => {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  const render = (mode: 'visible' | 'hidden', failed: boolean) =>
    root.render(
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
    await expect.element(page.getByText('Resource contents', { exact: true })).toBeVisible()
    const draft = page.getByRole('textbox', { name: 'Retained unsent message' })
    await userEvent.fill(draft, 'Keep the retained draft')
    const draftElement = container.querySelector('textarea')
    render('visible', true)
    await expect
      .element(page.getByRole('alert', { name: 'Resources rendering failure' }))
      .toBeVisible()
    render('visible', false)
    await expect
      .element(page.getByRole('alert', { name: 'Resources rendering failure' }))
      .toBeVisible()
    render('hidden', false)
    await expect.element(draftElement!).not.toBeVisible()
    render('visible', false)
    await expect.element(page.getByText('Resource contents', { exact: true })).toBeVisible()
    await expect
      .element(page.getByRole('alert', { name: 'Resources rendering failure' }))
      .not.toBeInTheDocument()
    await expect.element(draft).toHaveValue('Keep the retained draft')
    expect(container.querySelector('textarea')).toBe(draftElement)
  } finally {
    root.unmount()
    container.remove()
  }
})

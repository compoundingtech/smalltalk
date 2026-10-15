import * as React from 'react'
import type { Meta, StoryObj } from '@storybook/react'
import { expect, waitFor } from 'storybook/test'
import { coldOpenDocument } from './coldOpen.fixtures.ts'

const BootstrapSurface = ({ scheme = 'dark' }: { readonly scheme?: 'dark' | 'light' }) => <iframe title="Cold workspace" data-scheme={scheme} srcDoc={coldOpenDocument(undefined, scheme)} onLoad={event => { event.currentTarget.dataset.loaded = 'true' }} style={{ width: '100%', height: 900, border: 0 }} />
const meta = { title: 'Fractal/App/Shell/Cold open', component: BootstrapSurface, args: { scheme: 'dark' }, argTypes: { scheme: { control: 'inline-radio', options: ['dark', 'light'] } }, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof BootstrapSurface>
export default meta
type Story = StoryObj<typeof meta>
const assertBootstrap = (frame: HTMLIFrameElement): void => {
  const document = frame.contentDocument!
  const scheme = frame.dataset.scheme
  expect(document.documentElement.style.colorScheme).toBe(scheme)
  expect(frame.contentWindow!.getComputedStyle(document.body).backgroundColor).toBe(scheme === 'light' ? 'rgb(252, 252, 252)' : 'rgb(10, 10, 10)')
  expect(document.querySelectorAll('[data-wf-sidebar-skeleton] > div')).toHaveLength(8)
  // index.html caps the skeleton at max-width: 40vw, so narrow frames (440 px) get less than 256 px.
  expect(document.querySelector('[data-wf-sidebar-skeleton]')!.getBoundingClientRect().width).toBeCloseTo(Math.min(256, frame.contentWindow!.innerWidth * 0.4), 1)
}
const bootstrapPlay = async ({ canvasElement }: { readonly canvasElement: HTMLElement }) => {
  const frame = canvasElement.querySelector<HTMLIFrameElement>('iframe')!
  await waitFor(() => expect(frame.dataset.loaded).toBe('true'), { timeout: 90000 })
  assertBootstrap(frame)
  const document = frame.contentDocument!
  const bodyStyle = document.body.getAttribute('style')
  document.body.style.backgroundColor = 'white'
  try { await expect(() => assertBootstrap(frame)).toThrow() }
  finally {
    if (bodyStyle === null) document.body.removeAttribute('style')
    else document.body.setAttribute('style', bodyStyle)
  }
  const skeleton = document.querySelector('[data-wf-shell-skeleton]')!
  const parent = skeleton.parentNode!
  const next = skeleton.nextSibling
  skeleton.remove()
  try { await expect(() => assertBootstrap(frame)).toThrow() }
  finally { parent.insertBefore(skeleton, next) }
  assertBootstrap(frame)
  canvasElement.dataset.proof = 'passed'
}
export const DarkBeforeHydration: Story = { play: bootstrapPlay }
export const LightBeforeHydration: Story = { args: { scheme: 'light' }, play: bootstrapPlay }
export const AllStates: Story = DarkBeforeHydration

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within } from 'storybook/test'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

function ThinkingStory({ scheme = 'dark', streaming = false }: { scheme?: Scheme; streaming?: boolean }) {
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><ThinkingEntry text="Compare **reported rows** with the input, then ask a second worker to *review* the change." streaming={streaming} /></main>
}
const meta = { title: 'Fractal UI/Thinking Entry', component: ThinkingStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', streaming: false }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof ThinkingStory>
export default meta
type Story = StoryObj<typeof meta>
export const Settled: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const thinking = canvas.getByRole('button', { name: 'Thinking' })
  await expect(thinking).toHaveAttribute('aria-expanded', 'false')
  await expect(canvasElement.querySelector('[data-testid="markdown"]')).toBeNull()
  await userEvent.click(thinking)
  await expect(thinking).toHaveAttribute('aria-expanded', 'true')
  await expect(canvas.getByText('reported rows')).toHaveProperty('tagName', 'STRONG')
  await expect(canvas.getByText('review')).toHaveProperty('tagName', 'EM')
  const contentId = thinking.getAttribute('aria-controls')
  await expect(contentId).toBeTruthy()
  await expect(canvasElement.querySelector('[data-testid="markdown"]')?.parentElement?.id).toBe(contentId)
  await expect(canvasElement.querySelector('[data-streaming-tail="true"]')).toBeNull()
  await userEvent.keyboard('{Enter}')
  await expect(thinking).toHaveAttribute('aria-expanded', 'false')
  await expect(canvasElement.querySelector('[data-testid="markdown"]')).toBeNull()
} }
export const Streaming: Story = { args: { streaming: true }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const thinking = canvas.getByRole('button', { name: 'Thinking' })
  await expect(thinking).toHaveAttribute('aria-expanded', 'false')
  await userEvent.click(thinking)
  await expect(thinking).toHaveAttribute('aria-expanded', 'true')
  await expect(canvas.getByText('reported rows')).toHaveProperty('tagName', 'STRONG')
  await expect(canvasElement.querySelectorAll('[data-streaming-tail="true"]')).toHaveLength(1)
  await userEvent.keyboard('{Enter}')
  await expect(thinking).toHaveAttribute('aria-expanded', 'false')
  await expect(canvasElement.querySelector('[data-testid="markdown"]')).toBeNull()
} }
export const SettledLight: Story = { ...Settled, args: { scheme: 'light' } }
export const StreamingLight: Story = { ...Streaming, args: { scheme: 'light', streaming: true } }
export const AllStates: Story = { render: args => <main data-scheme={args.scheme} {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><ThinkingEntry text="Settled **analysis** stays collapsed until opened." /><ThinkingEntry text="Current *analysis* is still streaming." streaming /></main> }
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }
const styles = stylex.create({ root: { height: '100vh', boxSizing: 'border-box', padding: s.lg, display: 'flex', flexDirection: 'column', gap: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans } })

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect } from 'storybook/test'
import { surfaceVars as surface, textVars as text, typeVars as type } from './assistant-ui/composition-tokens.stylex'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { darkTheme } from './assistant-ui/embrace-theme'
import { DiffPanel } from './assistant-ui/composition/DiffPanel'
import { assertDiffTint } from './assistant-ui/composition/diff-tint-assertion'

const diff = ['@@ -1,2 +1,2 @@', ' const rows = [', '-  "previous row",', '+  "retained row",', ' ]']
const themes = { neutral: baselineTheme, composition: [], embrace: [...baselineTheme, darkTheme] } as const
function DiffTintCanvas({ theme = 'neutral', scheme = 'dark' }: { theme?: keyof typeof themes; scheme?: 'dark' | 'light' }) {
  return <div data-scheme={scheme} {...stylex.props(styles.canvas, ...themes[theme], ...(scheme === 'light' ? lightTheme : []))}><DiffPanel open width="100%" diff={diff} path="sample/rows.ts" added={1} removed={1} /></div>
}
const meta = { title: 'Fractal UI/Diff Tint', component: DiffTintCanvas, args: { theme: 'neutral', scheme: 'dark' }, parameters: { layout: 'fullscreen' }, play: async ({ canvasElement }) => {
  const receipt = assertDiffTint(canvasElement)
  canvasElement.dataset.diffTintReceipt = JSON.stringify(receipt)
  const counts = [...canvasElement.querySelectorAll('span')].filter(element => /^[-−+]1$/.test(element.textContent ?? ''))
  await expect(counts.length).toBeGreaterThan(0)
  for (const count of counts) {
    const values = getComputedStyle(count).color.match(/[\d.]+/g)!.slice(0, 3).map(Number)
    // The light muted-foreground token has an 8/255 blue offset; it is still near-achromatic.
    await expect(Math.max(...values) - Math.min(...values)).toBeLessThanOrEqual(10)
  }
} } satisfies Meta<typeof DiffTintCanvas>
export default meta
type Story = StoryObj<typeof meta>
export const Neutral: Story = {}
export const NeutralLight: Story = { args: { scheme: 'light' } }
export const Composition: Story = { args: { theme: 'composition' } }
export const CompositionLight: Story = { args: { theme: 'composition', scheme: 'light' } }
export const Embrace: Story = { args: { theme: 'embrace' } }
export const EmbraceLight: Story = { args: { theme: 'embrace', scheme: 'light' } }
const styles = stylex.create({ canvas: { height: '100vh', backgroundColor: surface.canvas, color: text.fg, fontFamily: type.fontSans, fontSize: type.uiSize, lineHeight: type.uiLeading } })

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect } from 'storybook/test'
import { colorVars as color, statusVars as status, surfaceVars as surface, textVars as text, typeVars as type } from './assistant-ui/composition-tokens.stylex'
import { tokens } from './assistant-ui/embrace-tokens.stylex'
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
const meta = { title: 'Fractal UI/Diff Tint', component: DiffTintCanvas, args: { theme: 'neutral', scheme: 'dark' }, parameters: { layout: 'fullscreen' }, play: async ({ canvasElement, args }) => {
  const receipt = assertDiffTint(canvasElement)
  canvasElement.dataset.diffTintReceipt = JSON.stringify(receipt)
  const counts = [...canvasElement.querySelectorAll('span')].filter(element => /^[-−+]1$/.test(element.textContent ?? ''))
  await expect(counts.length).toBeGreaterThan(0)
  for (const count of counts) {
    const values = getComputedStyle(count).color.match(/[\d.]+/g)!.slice(0, 3).map(Number)
    // The light muted-foreground token has an 8/255 blue offset; it is still near-achromatic.
    await expect(Math.max(...values) - Math.min(...values)).toBeLessThanOrEqual(10)
  }
  // diffAdded aliases the muted foreground, so the addition count paints exactly like the deletion count.
  const [addedCount, removedCount] = ['+1', '−1'].map(value => counts.find(element => element.textContent === value)!)
  const countsMatch = () => getComputedStyle(addedCount!).color === getComputedStyle(removedCount!).color
  await expect(countsMatch()).toBe(true)
  // Control: the other scheme's muted ink, i.e. an alias resolved against the wrong scheme.
  addedCount!.style.color = args.scheme === 'light' ? '#818181' : '#66666e'
  await expect(countsMatch()).toBe(false)
  addedCount!.style.removeProperty('color')
  await expect(countsMatch()).toBe(true)
  // Embrace's good/added must resolve to this scheme's soft ink and addition tint wherever the Embrace palette
  // matches the scheme: light without darkTheme (the :root defaults) and any subtree carrying darkTheme.
  const scope = canvasElement.querySelector<HTMLElement>('[data-scheme]')!
  const paint = (token: string) => {
    const probe = document.createElement('span')
    probe.style.color = getComputedStyle(scope).getPropertyValue(token.slice(4, -1)).trim()
    scope.append(probe)
    const value = getComputedStyle(probe).color
    probe.remove()
    return value
  }
  // C18 v1.1: secondary ink uses muted, not a fourth grey. Restore the retired alpha as a failing control.
  for (const token of [text.fgMuted, color.fgMuted]) {
    const property = token.slice(4, -1)
    const original = scope.style.getPropertyValue(property)
    const expected = args.scheme === 'light' ? 'rgb(102, 102, 110)' : 'rgb(129, 129, 129)'
    await expect(paint(token)).toBe(expected)
    scope.style.setProperty(property, args.scheme === 'light' ? 'rgba(39, 39, 42, 0.55)' : 'rgba(245, 245, 245, 0.52)')
    await expect(paint(token)).not.toBe(expected)
    if (original) scope.style.setProperty(property, original); else scope.style.removeProperty(property)
    await expect(paint(token)).toBe(expected)
  }
  const aliases = () => [paint(tokens.good) === paint(text.fgSoft), paint(tokens.added) === paint(status.diffAddTint)]
  if (args.scheme === 'light' || args.theme === 'embrace') {
    await expect(aliases()).toEqual([true, true])
    // Control: the opposite scheme's values, which is what a frozen :root alias leaves in place.
    const [good, added] = args.scheme === 'light' ? ['rgba(245, 245, 245, 0.8)', '#7295ed'] : ['rgba(39, 39, 42, 0.82)', '#4269df']
    scope.style.setProperty(tokens.good.slice(4, -1), good)
    scope.style.setProperty(tokens.added.slice(4, -1), added)
    await expect(aliases()).toEqual([false, false])
    scope.style.removeProperty(tokens.good.slice(4, -1))
    scope.style.removeProperty(tokens.added.slice(4, -1))
    await expect(aliases()).toEqual([true, true])
  }
  const row = canvasElement.querySelector<HTMLElement>('[data-diff-kind="added"]')!
  const gutter = row.querySelector<HTMLElement>('[data-diff-gutter]')!
  const original = [row, gutter].map(element => element.getAttribute('style'))
  try {
    // Actual pre-tint route paint: a neutral wash and a separate muted gutter.
    row.style.backgroundColor = args.scheme === 'light' ? 'rgba(128, 128, 128, 0.08)' : 'rgba(128, 128, 128, 0.09)'
    gutter.style.borderLeftColor = getComputedStyle(canvasElement).getPropertyValue(text.fgMuted.slice(4, -1))
    await expect(() => assertDiffTint(canvasElement)).toThrow('Added wash and gutter do not share a semantic token')
    // Equal blue pixels from unrelated variables are not a shared semantic token.
    row.style.setProperty('--independent-addition-ink', getComputedStyle(gutter).getPropertyValue(receipt.gutterToken!))
    row.style.backgroundColor = 'color-mix(in srgb, var(--independent-addition-ink) 9%, transparent)'
    gutter.style.removeProperty('border-left-color')
    await expect(() => assertDiffTint(canvasElement)).toThrow('Added wash and gutter do not share a semantic token')
  } finally {
    for (const [index, element] of [row, gutter].entries()) {
      if (original[index] === null) element.removeAttribute('style')
      else element.setAttribute('style', original[index]!)
    }
  }
  await expect(assertDiffTint(canvasElement)).toEqual(receipt)
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

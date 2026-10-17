/** Resource card disclosure (keyboard path): the custom hover is the only hover disclosure (no native titles), and the modal keeps the full detail as its accessible description. */
import type { Meta, StoryObj } from '@storybook/react-vite'
import * as stylex from '@stylexjs/stylex'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { ResourceCardV1 } from './ResourceCardV1'
import type { ResourceData } from './resource-model'
import { spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t, geometryVars as g } from '../composition-tokens.stylex'
import { baselineTheme } from '../neutral-theme'
import { lightTheme } from '../composition-theme'
import { ThemePortal } from '../taste/ThemePortal'

/** Synthetic fully reported diff; every count and preview is known. */
const resource: ResourceData = {
  kind: 'diff', label: 'Row naming diff', detail: 'feat/rows · 2 files changed against the synthetic base revision for row naming',
  fileCount: { _tag: 'Known', value: 2 },
  files: [
    { path: 'src/rows/name.ts', added: { _tag: 'Known', value: 4 }, removed: { _tag: 'Known', value: 1 }, lines: { _tag: 'Known', value: ['+export const name = title', '-export const name = label', ' // unchanged'] } },
    { path: 'src/rows/description.ts', added: { _tag: 'Known', value: 2 }, removed: { _tag: 'Known', value: 0 }, lines: { _tag: 'Known', value: ['+export const description = facts'] } },
  ],
}

interface Args { scheme: 'dark' | 'light' }
const meta = {
  title: 'Fractal UI/Resources/Resource Card',
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Resource card hover and standalone dialog: the hover card is the only hover disclosure and the dialog keeps the full detail as its description.' } } },
  args: { scheme: 'dark' },
  argTypes: { scheme: { control: 'inline-radio', options: ['dark', 'light'] } },
} satisfies Meta<Args>
export default meta
type Story = StoryObj<Args>

export const HoverAndDialog: Story = { name: 'Hover and dialog · no native titles', render: ({ scheme }) => <main {...stylex.props(styles.root, ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}><ThemePortal><div {...stylex.props(styles.frame)}><ResourceCardV1 resource={resource} variant="RC-2" /></div></ThemePortal></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvasElement.querySelectorAll('[title]')).toHaveLength(0)
  // Label-in-name: every resource button is named by its own visible content.
  for (const button of canvas.getAllByRole('button')) await expect(button).not.toHaveAttribute('aria-label')
  const trigger = canvas.getByRole('button', { name: /^2 changed files\b/ })
  // Keyboard focus opens the custom hover immediately (RAC tooltips open on focus-visible without the hover warmup).
  await userEvent.tab()
  await expect(trigger).toHaveFocus()
  const tooltip = await waitFor(() => { const found = document.querySelector<HTMLElement>('[role="tooltip"]'); expect(found).not.toBeNull(); return found! })
  await expect(tooltip.querySelector('[data-resource-detail]')!.textContent).toBe(resource.detail)
  await expect(tooltip.querySelectorAll('[title]')).toHaveLength(0)
  for (let node: HTMLElement | null = trigger; node !== null; node = node.parentElement) await expect(node.hasAttribute('title')).toBe(false)
  await expect(document.querySelectorAll('[role="tooltip"]')).toHaveLength(1)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(document.querySelector('[role="tooltip"]')).toBeNull())
  await userEvent.keyboard('{Enter}')
  const dialog = await waitFor(() => { const found = document.querySelector<HTMLElement>('[role="dialog"]'); expect(found).not.toBeNull(); return found! })
  await expect(dialog).toHaveAccessibleName('Resource 2 changed files')
  await expect(dialog).toHaveAccessibleDescription(resource.detail)
  await expect(dialog.querySelector('[data-resource-detail]')!.textContent).toBe(resource.detail)
  await expect(dialog.querySelectorAll('[title]')).toHaveLength(0)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(document.querySelector('[role="dialog"]')).toBeNull())
} }

const styles = stylex.create({
  root: { height: '100vh', minHeight: 0, overflowY: 'auto', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  frame: { width: g.specimenAside, maxWidth: '100%' },
})

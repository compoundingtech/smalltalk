import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react'
import { Button } from 'react-aria-components'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { SidePanel, assistantDarkTheme, compositionLightTheme, liveAccentTheme } from './assistant-ui/shell.ts'
import { colorVars as c, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex.ts'

const PanelSurface = ({ scheme = 'dark', width = 1440, initiallyOpen = false }: { readonly scheme?: 'dark' | 'light'; readonly width?: number; readonly initiallyOpen?: boolean }) => {
  const [open, setOpen] = React.useState(initiallyOpen)
  const panelId = React.useId()
  return <div data-side-panel-story="" data-scheme={scheme} {...stylex.props(scheme === 'dark' && assistantDarkTheme, liveAccentTheme, scheme === 'light' && compositionLightTheme, styles.surface)} style={{ width: Math.min(width, window.innerWidth) }}>
    <main aria-label="Workspace" {...stylex.props(styles.workspace)}>
      <header {...stylex.props(styles.header)}>
        <strong>Builder session</strong>
        <Button aria-label="Resources" aria-expanded={open} aria-controls={open ? panelId : undefined} onPress={() => setOpen(value => !value)} {...stylex.props(styles.button)}>Resources</Button>
      </header>
      <ol {...stylex.props(styles.transcript)}>{Array.from({ length: 24 }, (_, index) => <li key={index}>Transcript entry {index + 1}: the workspace stays readable and interactive while the panel is open.</li>)}</ol>
      <Button {...stylex.props(styles.button)}>Workspace action</Button>
    </main>
    {open && <SidePanel id={panelId} aria-label="Agent resources" onClose={() => setOpen(false)}>
      <header {...stylex.props(styles.panelHeader)}><strong>Resources</strong><Button aria-label="Close resources" onPress={() => setOpen(false)} {...stylex.props(styles.button)}>Close resources</Button></header>
      <div {...stylex.props(styles.panelBody)}>
        <section aria-label="Todos"><h2 {...stylex.props(styles.title)}>Todos</h2><p>No harness todo snapshot observed.</p></section>
        <section aria-label="Observed resources"><h2 {...stylex.props(styles.title)}>Observed resources</h2><ul>{['Project notes', 'Build checkout', 'Review thread'].map(name => <li key={name}>{name}</li>)}</ul></section>
      </div>
    </SidePanel>}
  </div>
}

const nonModalPlay = async ({ canvasElement }: { readonly canvasElement: HTMLElement }) => {
  const root = canvasElement.querySelector<HTMLElement>('[data-side-panel-story]')!
  const canvas = within(root)
  const opener = canvas.getByRole('button', { name: 'Resources' })
  await userEvent.click(opener)
  const panel = await canvas.findByRole('complementary', { name: 'Agent resources' })
  await expect(opener).toHaveAttribute('aria-expanded', 'true')
  await expect(opener).toHaveAttribute('aria-controls', panel.id)
  await waitFor(() => expect(panel.contains(document.activeElement)).toBe(true))
  // Non-modal: the workspace is neither inert nor hidden, and stays operable.
  await expect(root.ownerDocument.querySelectorAll('[inert]').length).toBe(0)
  await expect(canvas.getByRole('main', { name: 'Workspace' }).closest('[aria-hidden="true"]')).toBeNull()
  await expect(canvas.queryByRole('dialog')).toBeNull()
  // A child that handles Escape itself keeps the panel open.
  const close = within(panel).getByRole('button', { name: 'Close resources' })
  const childEscape = (event: KeyboardEvent) => { if (event.key === 'Escape') event.preventDefault() }
  close.addEventListener('keydown', childEscape)
  try {
    close.focus()
    await userEvent.keyboard('{Escape}')
    await expect(panel).toBeInTheDocument()
  } finally { close.removeEventListener('keydown', childEscape) }
  await userEvent.keyboard('{Escape}')
  await expect(canvas.queryByRole('complementary', { name: 'Agent resources' })).toBeNull()
  await waitFor(() => expect(opener).toHaveFocus())
  await expect(opener).toHaveAttribute('aria-expanded', 'false')
  root.dataset.proof = 'passed'
}

// The kit gate runs every story at 440/1440 in both schemes and passes `scheme` as an arg; `width` clamps to the viewport.
const meta = { title: 'Fractal/Shell/Side panel', component: PanelSurface, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', width: 1440 } } satisfies Meta<typeof PanelSurface>
export default meta
type Story = StoryObj<typeof meta>
export const NonModal: Story = { play: nonModalPlay }
export const Open: Story = { args: { initiallyOpen: true } }

const styles = stylex.create({
  surface: { position: 'relative', height: 900, display: 'flex', backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  workspace: { flex: '1 1 0', minWidth: 0, display: 'flex', flexDirection: 'column', gap: s.md, padding: s.lg, overflowY: 'auto' },
  header: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: s.md },
  transcript: { margin: 0, paddingInlineStart: s.lg, display: 'flex', flexDirection: 'column', gap: s.sm },
  button: { paddingBlock: s.xs, paddingInline: s.md, borderWidth: 0, borderRadius: 6, backgroundColor: c.rowHover, color: c.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${c.primary}` } },
  panelHeader: { display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: s.lg, borderBottom: `1px solid ${c.border}` },
  panelBody: { flex: '1 1 0', minHeight: 0, padding: s.lg, overflowY: 'auto' },
  title: { fontSize: t.uiSize, marginBlock: s.md },
})

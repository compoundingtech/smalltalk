import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, within } from 'storybook/test'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { colorVars as c, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const terminalReason = 'The terminal opens once this agent is running on a host.'
function ThreadHeaderStory({ scheme = 'dark', terminalDisabledReason, withoutFolder = false }: { scheme?: Scheme; terminalDisabledReason?: string; withoutFolder?: boolean }) {
  // Match the app binding: a host observation may be absent.
  const agent: { host?: string } | undefined = withoutFolder ? undefined : { host: 'fractal' }
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <ThreadHeader folder={agent?.host} title="Row projection" panelOpen={false} drawerOpen={false} onTogglePanel={() => {}} onToggleDrawer={() => {}} terminalDisabledReason={terminalDisabledReason} />
  </main>
}
const meta = { title: 'Fractal UI/Thread header', component: ThreadHeaderStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof ThreadHeaderStory>
export default meta
type Story = StoryObj<typeof meta>
/** The host knows why the terminal is unavailable: the toggle stays in place, disabled and described. */
export const TerminalDisabled: Story = { args: { terminalDisabledReason: terminalReason }, play: async ({ canvasElement }) => {
  const header = within(canvasElement).getByTestId('thread-header')
  const toggle = within(header).getByRole('button', { name: 'Toggle terminal drawer' })
  await expect(toggle).toBeVisible()
  await expect(toggle).toBeDisabled()
  await expect(toggle).toHaveAccessibleName('Toggle terminal drawer')
  await expect(toggle).toHaveAccessibleDescription(terminalReason)
  await expect(within(header).getAllByRole('button').map(button => button.getAttribute('aria-label'))).toEqual(['Toggle terminal drawer', 'Toggle right panel'])
  await expect(toggle.closest('[title]')).toHaveAttribute('title', terminalReason)
} }
export const TerminalDisabledLight: Story = { ...TerminalDisabled, args: { ...TerminalDisabled.args, scheme: 'light' } }
/** Without a reason the unavailable terminal leaves no toggle behind. */
export const TerminalHidden: Story = { play: async ({ canvasElement }) => {
  const header = within(canvasElement).getByTestId('thread-header')
  await expect(within(header).queryByRole('button', { name: 'Toggle terminal drawer' })).toBeNull()
  await expect(within(header).getByRole('button', { name: 'Toggle right panel' })).toBeVisible()
  await expect(within(header).queryByText(terminalReason)).toBeNull()
} }
export const TerminalHiddenLight: Story = { ...TerminalHidden, args: { scheme: 'light' } }
/** An unreported host folder leaves only the title, never an empty crumb or separator. */
export const WithoutFolder: Story = { args: { withoutFolder: true }, play: async ({ canvasElement }) => {
  const breadcrumb = within(canvasElement).getByRole('navigation', { name: 'Breadcrumb' })
  await expect(breadcrumb).toHaveTextContent('Row projection')
  await expect(breadcrumb.children).toHaveLength(1)
  await expect(breadcrumb.querySelector('[aria-hidden]')).toBeNull()
  await expect(breadcrumb.textContent).not.toMatch(/\/|Unknown/i)
} }
export const WithoutFolderLight: Story = { ...WithoutFolder, args: { ...WithoutFolder.args, scheme: 'light' } }
const styles = stylex.create({
  root: { minHeight: '100vh', boxSizing: 'border-box', backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans },
})

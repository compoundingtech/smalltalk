import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { configure, expect, fireEvent, userEvent as events, waitFor, within } from 'storybook/test'
import { CommandsProvider, CommandButton, useCommands, type CommandPlatform, type KitCommand } from './assistant-ui/commands'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { Composer } from './assistant-ui/composition/Composer'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, typeVars as t, spaceVars as s } from './assistant-ui/composition-tokens.stylex'

/** RAC collections can suspend on open; await their React act scope instead of DOM's synchronous event wrapper. */
async function interact(action: () => void | Promise<unknown>) {
  let restoreConfig = () => {}
  configure(current => {
    restoreConfig = () => configure(current)
    return { asyncWrapper: callback => callback(), eventWrapper: callback => callback() }
  })
  const environment = globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }
  const previous = environment.IS_REACT_ACT_ENVIRONMENT
  environment.IS_REACT_ACT_ENVIRONMENT = true
  try { await React.act(async () => { await action() }) }
  finally { restoreConfig(); environment.IS_REACT_ACT_ENVIRONMENT = previous }
}
const withAct = <Args extends unknown[],>(action: (...args: Args) => Promise<unknown>) => (...args: Args) => interact(() => action(...args))
const userEvent = { click: withAct(events.click), keyboard: withAct(events.keyboard), type: withAct(events.type), clear: withAct(events.clear), tab: withAct(events.tab), hover: withAct(events.hover), unhover: withAct(events.unhover) }

type Control = 'none' | 'perform' | 'disabled' | 'help' | 'tooltip' | 'platform' | 'composition'
function Discovery() {
  const registry = useCommands()!
  return <div {...stylex.props(styles.tools)}><CommandButton label="Find command" onPress={registry.openPalette}>Find command</CommandButton><CommandButton label="Show shortcuts" onPress={registry.openShortcuts}>Show shortcuts</CommandButton></div>
}
function CommandsStory({ scheme = 'dark', platform = 'other', control = 'none' }: { scheme?: Scheme; platform?: CommandPlatform; control?: Control }) {
  const [performed, setPerformed] = React.useState<readonly string[]>([])
  const [panel, setPanel] = React.useState(false)
  const [terminal, setTerminal] = React.useState(false)
  const field = React.useRef<HTMLTextAreaElement>(null)
  const receipt = (label: string) => { if (control !== 'perform') setPerformed(previous => [...previous, label]) }
  const commands: readonly KitCommand[] = [
    { id: 'layout.panel', label: 'Toggle right panel', group: control === 'help' ? 'Control group' : 'Workspace', shortcut: { key: 'd', modifiers: ['Mod', 'Shift'] }, perform: () => { setPanel(value => !value); receipt('Panel toggled') } },
    { id: 'layout.terminal', label: 'Toggle terminal drawer', group: control === 'help' ? 'Control group' : 'Workspace', shortcut: { mac: { key: 'j', modifiers: ['Mod'] }, other: { key: 'j', modifiers: ['Mod', 'Shift'] } }, perform: () => { setTerminal(value => !value); receipt('Terminal toggled') } },
    { id: 'review.publish', label: 'Publish review', group: 'Review', shortcut: { key: 'p', modifiers: ['Mod', 'Shift'] }, when: () => control === 'disabled' ? true : 'Connect a review destination first.', perform: () => receipt('Review published') },
    { id: 'composer.send', label: 'Send message', group: 'Composer', shortcut: { key: 'Enter', modifiers: ['Mod'] }, allowInEditable: true, when: () => field.current?.value.trim() ? true : 'Write a message first.', perform: () => receipt('Message sent') },
    { id: 'composer.focus', label: 'Focus composer', group: 'Composer', shortcut: { key: 'l', modifiers: ['Mod'] }, perform: () => field.current?.focus() },
  ]
  return <main data-scheme={scheme} onCompositionStartCapture={event => { if (control === 'composition') event.stopPropagation() }} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <CommandsProvider commands={commands} platform={control === 'platform' ? platform === 'mac' ? 'other' : 'mac' : platform}>
      <div>
        <ThreadHeader folder="fractal" title="Command workspace" terminalAvailable panelOpen={panel} drawerOpen={terminal} onTogglePanel={() => receipt('Callback path')} onToggleDrawer={() => receipt('Callback path')} commandIds={control === 'tooltip' ? {} : { panel: 'layout.panel', terminal: 'layout.terminal' }} />
        <Discovery />
        <div {...stylex.props(styles.editor)}>
          <EmbraceRuntimeProvider options={{ messages: [], onNew: async () => receipt('Message sent') }}>
            <Composer agent="Review agent" folder="fractal" draftKey={`commands.${platform}.${scheme}.${control}`} inputRef={field} onSend={() => receipt('Message sent')} commandIds={{ send: 'composer.send' }} />
          </EmbraceRuntimeProvider>
        </div>
        <div role="log" aria-label="Performed commands" {...stylex.props(styles.log)}>{performed.map((label, index) => <p key={index}>{label}</p>)}</div>
      </div>
    </CommandsProvider>
  </main>
}
const meta = { title: 'Fractal UI/Commands', component: CommandsStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', platform: 'other', control: 'none' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, platform: { options: ['mac', 'other'], control: 'radio' }, control: { table: { disable: true } } } } satisfies Meta<typeof CommandsStory>
export default meta
type Story = StoryObj<typeof meta>
const modK = async (platform: CommandPlatform = 'other') => { await userEvent.keyboard(platform === 'mac' ? '{Meta>}k{/Meta}' : '{Control>}k{/Control}') }
const dialog = (canvasElement: HTMLElement) => within(canvasElement.ownerDocument.body)
export const PaletteFlow: Story = { play: async ({ canvasElement, args }) => {
  const canvas = within(canvasElement)
  const trigger = canvas.getByRole('button', { name: 'Find command' })
  await interact(() => trigger.focus())
  await modK(args.platform)
  const page = dialog(canvasElement)
  const search = page.getByRole('searchbox', { name: 'Search commands' })
  await expect(search).toHaveFocus()
  await expect(page.getByRole('menuitem', { name: /Toggle terminal drawer/ })).toBeVisible()
  await userEvent.type(search, 'tgrpnl')
  await expect(page.getByRole('menuitem', { name: /Toggle right panel/ })).toBeVisible()
  await expect(page.queryByRole('menuitem', { name: /Toggle terminal drawer/ })).toBeNull()
  await userEvent.keyboard('{ArrowDown}{Enter}')
  await waitFor(() => expect(page.queryByRole('dialog')).toBeNull())
  await waitFor(() => expect(trigger).toHaveFocus(), { timeout: 5000 })
  await expect(canvas.getByRole('log')).toHaveTextContent('Panel toggled')
  await expect(canvas.getByRole('log')).not.toHaveTextContent('Callback path')
  await expect(canvas.getByRole('button', { name: 'Toggle right panel' })).toHaveAttribute('aria-pressed', 'true')
  await modK(args.platform)
  await userEvent.type(page.getByRole('searchbox'), 'terminal')
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(trigger).toHaveFocus(), { timeout: 5000 })
  await expect(canvas.getByRole('log').children).toHaveLength(1)
} }
export const PaletteFlowLight: Story = { ...PaletteFlow, args: { scheme: 'light' } }
export const PaletteFlowMac: Story = { ...PaletteFlow, args: { platform: 'mac' } }
export const DisabledCommand: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Find command' }))
  const page = dialog(canvasElement)
  const search = page.getByRole('searchbox')
  await userEvent.type(search, 'pubrev')
  const row = page.getByRole('menuitem', { name: /Publish review/ })
  await expect(row).toHaveAttribute('aria-disabled', 'true')
  await expect(row).toHaveAccessibleDescription('Connect a review destination first.')
  await expect(page.getByText('Connect a review destination first.')).toBeVisible()
  await userEvent.click(row)
  await userEvent.keyboard('{ArrowDown}{Enter}')
  await expect(page.getByRole('dialog')).toBeVisible()
  await expect(canvas.getByRole('log').children).toHaveLength(0)
  await userEvent.keyboard('{Escape}')
} }
export const DisabledCommandLight: Story = { ...DisabledCommand, args: { scheme: 'light' } }
export const ShortcutHelp: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const composer = canvas.getByRole('textbox', { name: 'Message' })
  await userEvent.click(composer)
  await userEvent.keyboard('?')
  await expect(dialog(canvasElement).queryByRole('dialog')).toBeNull()
  await expect(composer).toHaveValue('?')
  await userEvent.clear(composer)
  const trigger = canvas.getByRole('button', { name: 'Show shortcuts' })
  await interact(() => trigger.focus())
  await userEvent.keyboard('?')
  const page = dialog(canvasElement)
  await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Workspace' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Composer' })).toBeVisible()
  await expect(page.getByText('Ctrl+Shift+D')).toBeVisible()
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(trigger).toHaveFocus())
  await userEvent.click(canvas.getByRole('button', { name: 'Find command' }))
  await userEvent.keyboard('{Escape}')
  await interact(() => (canvasElement.ownerDocument.activeElement as HTMLElement).blur())
  await userEvent.keyboard('?')
  await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible()
  await userEvent.keyboard('{Escape}')
} }
export const ShortcutHelpLight: Story = { ...ShortcutHelp, args: { scheme: 'light' } }
export const Tooltips: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const toggle = canvas.getByRole('button', { name: 'Toggle right panel' })
  await interact(() => canvas.getByRole('button', { name: 'Find command' }).focus())
  await userEvent.tab({ shift: true })
  await expect(toggle).toHaveFocus()
  const page = dialog(canvasElement)
  await waitFor(() => expect(page.getByRole('tooltip')).toHaveTextContent('Toggle right panelCtrl+Shift+D'))
  await userEvent.tab()
  await interact(() => (canvasElement.ownerDocument.activeElement as HTMLElement).blur())
  await waitFor(() => expect(page.queryByRole('tooltip')).toBeNull())
  await userEvent.hover(toggle)
  await waitFor(() => expect(page.getByRole('tooltip')).toHaveTextContent('Toggle right panelCtrl+Shift+D'))
  await userEvent.unhover(toggle)
  await userEvent.type(canvas.getByRole('textbox', { name: 'Message' }), 'Review generated changes')
  await userEvent.hover(canvas.getByRole('button', { name: 'Send' }))
  await waitFor(() => expect(page.getByRole('tooltip')).toHaveTextContent('Send messageCtrl+Enter'))
  await userEvent.unhover(canvas.getByRole('button', { name: 'Send' }))
} }
export const TooltipsLight: Story = { ...Tooltips, args: { scheme: 'light' } }
export const PlatformShortcuts: Story = { args: { platform: 'mac' }, play: async ({ canvasElement, args }) => {
  await userEvent.click(within(canvasElement).getByRole('button', { name: 'Find command' }))
  const page = dialog(canvasElement)
  const expected = args.platform === 'mac' ? 'Cmd+J' : 'Ctrl+Shift+J'
  const wrong = args.platform === 'mac' ? 'Ctrl+Shift+J' : 'Cmd+J'
  await expect(page.getByText(expected)).toBeVisible()
  await expect(page.queryByText(wrong)).toBeNull()
  await userEvent.keyboard('{Escape}')
} }
export const PlatformShortcutsOther: Story = { ...PlatformShortcuts, args: { platform: 'other', scheme: 'light' } }
export const CompositionAndInputs: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const composer = canvas.getByRole('textbox', { name: 'Message' })
  await userEvent.click(composer)
  await interact(() => fireEvent.compositionStart(composer))
  await userEvent.keyboard('{Control>}k{/Control}')
  await expect(dialog(canvasElement).queryByRole('dialog')).toBeNull()
  await interact(() => fireEvent.compositionEnd(composer))
  await interact(() => fireEvent.keyDown(composer, { key: 'k', ctrlKey: true, keyCode: 229 }))
  await expect(dialog(canvasElement).queryByRole('dialog')).toBeNull()
  await interact(() => fireEvent.keyDown(composer, { key: 'k', ctrlKey: true, repeat: true }))
  await expect(dialog(canvasElement).queryByRole('dialog')).toBeNull()
  await userEvent.keyboard('{Control>}{Shift>}d{/Shift}{/Control}')
  await expect(canvas.getByRole('log').children).toHaveLength(0)
  await modK()
  await expect(dialog(canvasElement).getByRole('searchbox')).toHaveFocus()
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(composer).toHaveFocus())
  await userEvent.type(composer, 'Generated message')
  await userEvent.keyboard('{Control>}{Enter}{/Control}')
  await expect(canvas.getByRole('log').children).toHaveLength(1)
  await expect(canvas.getByRole('log')).toHaveTextContent('Message sent')
} }
export const CompositionAndInputsLight: Story = { ...CompositionAndInputs, args: { scheme: 'light' } }
export const AllStates: Story = { ...DisabledCommand, play: async context => {
  await DisabledCommand.play?.(context)
  await userEvent.click(within(context.canvasElement).getByRole('button', { name: 'Find command' }))
  const page = dialog(context.canvasElement)
  await expect(page.getAllByRole('menuitem')).toHaveLength(5)
  await expect(page.getByRole('menuitem', { name: /Toggle right panel/ })).not.toHaveAttribute('aria-disabled', 'true')
  await expect(page.getByRole('menuitem', { name: /Publish review/ })).toHaveAttribute('aria-disabled', 'true')
  await expect(page.getByText('Write a message first.')).toBeVisible()
} }
const styles = stylex.create({
  root: { minHeight: '100vh', boxSizing: 'border-box', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize },
  tools: { display: 'flex', gap: s.lg, padding: s.xl },
  editor: { maxWidth: '736px', padding: s.xl },
  log: { paddingInline: s.xl, fontSize: t.metaSize },
})

import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Dialog, DialogTrigger, Menu, MenuItem, MenuTrigger, Modal, Popover } from 'react-aria-components'
import { expect, fn, userEvent, waitFor, within } from 'storybook/test'
import { ChromeControl } from './assistant-ui/composition/ChromeControl'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

/**
 * The chrome's quiet icon controls in place: the sidebar header and footer, and the thread header with the
 * agent, resources and conversation controls beside the terminal and panel toggles (panel open, so pressed).
 * Resources is a dialog trigger and Conversation menu a menu trigger, as in the app; each is disabled when
 * given a reason. Below 640px the sidebar strip stacks above the header.
 */
function ChromeControlsStory({ scheme = 'dark', resourcesDisabledReason, conversationDisabledReason, onResources }: { scheme?: Scheme; resourcesDisabledReason?: string; conversationDisabledReason?: string; onResources?: () => void }) {
  const [collapsed, setCollapsed] = React.useState(false)
  const [panelOpen, setPanelOpen] = React.useState(true)
  const [drawerOpen, setDrawerOpen] = React.useState(false)
  const [needsMe, setNeedsMe] = React.useState(false)
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <aside aria-label="Agents" {...stylex.props(styles.sidebar)}>
      <header {...stylex.props(styles.band)}>
        <ChromeControl icon={collapsed ? 'chevron-right' : 'chevrons-left'} label={collapsed ? 'Expand agents' : 'Collapse agents'} onPress={() => setCollapsed(value => !value)} />
        <strong>Fractal</strong>
        <span {...stylex.props(styles.push)} />
        <ChromeControl icon="bell" label="Needs me only" pressed={needsMe} onPress={() => setNeedsMe(value => !value)} />
        <ChromeControl icon="filter" label="Filter agents · 1 active" active />
        <ChromeControl icon="sort" label="Sort agents · Manual" />
      </header>
      <footer {...stylex.props(styles.band)}>
        <ChromeControl icon="swap" label="Toggle color scheme" tooltipPlacement="top" />
        <ChromeControl icon="checklist" label="Missions" tooltipPlacement="top" />
        <ChromeControl icon="gauge" label="Usage" tooltipPlacement="top" />
      </footer>
    </aside>
    <section aria-label="Agent workspace" {...stylex.props(styles.workspace)}>
      <ThreadHeader folder="workstation-a" title="fractal-web" status="working" statusLabel="working" panelOpen={panelOpen} drawerOpen={drawerOpen} onTogglePanel={() => setPanelOpen(value => !value)} onToggleDrawer={() => setDrawerOpen(value => !value)} terminalAvailable nativeActions={
        <div {...stylex.props(styles.actions)}>
          <ChromeControl icon="person" label="Actions for fractal-web" />
          <DialogTrigger>
            <ChromeControl icon="folder" label="Resources" {...(resourcesDisabledReason === undefined ? {} : { disabledReason: resourcesDisabledReason })} {...(onResources === undefined ? {} : { onPress: onResources })} />
            <Modal isDismissable {...stylex.props(styles.modal)}><Dialog aria-label="Agent resources" {...stylex.props(styles.dialog)}>Agent resources</Dialog></Modal>
          </DialogTrigger>
          <MenuTrigger>
            <ChromeControl icon="gear" label="Conversation menu" {...(conversationDisabledReason === undefined ? {} : { disabledReason: conversationDisabledReason })} />
            <Popover {...stylex.props(styles.popover)}><Menu aria-label="Conversation settings" {...stylex.props(styles.menu)}><MenuItem id="system">Show all system events</MenuItem></Menu></Popover>
          </MenuTrigger>
        </div>
      } />
    </section>
  </main>
}

const meta = {
  title: 'Fractal UI/Chrome controls',
  component: ChromeControlsStory,
  parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', onResources: fn() },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } },
} satisfies Meta<typeof ChromeControlsStory>
export default meta
type Story = StoryObj<typeof meta>

const header = (canvasElement: HTMLElement) => within(canvasElement).getByTestId('thread-header')
const headerNames = ['Actions for fractal-web', 'Resources', 'Conversation menu', 'Toggle terminal drawer', 'Toggle right panel']

/** Every chrome control is a 28px icon button named by its accessible name; open toggles and set filters keep the fill. */
export const QuietIcons: Story = { play: async ({ canvasElement }) => {
  const buttons = within(header(canvasElement)).getAllByRole('button')
  await expect(buttons.map(button => button.getAttribute('aria-label'))).toEqual(headerNames)
  for (const button of within(canvasElement).getAllByRole('button')) {
    await expect(button.textContent?.trim()).toBe('')
    await expect(button.querySelectorAll('svg')).toHaveLength(1)
    await expect(button.getBoundingClientRect().height).toBe(28)
  }
  await expect(within(header(canvasElement)).getByRole('button', { name: 'Toggle right panel' })).toHaveAttribute('aria-pressed', 'true')
  await expect(within(header(canvasElement)).getByRole('button', { name: 'Toggle terminal drawer' })).toHaveAttribute('aria-pressed', 'false')
  const sidebar = within(within(canvasElement).getByRole('complementary', { name: 'Agents' }))
  await expect(sidebar.getByRole('button', { name: 'Filter agents · 1 active' })).not.toHaveAttribute('aria-pressed')
  await expect(sidebar.getAllByRole('button').map(button => button.getAttribute('aria-label'))).toEqual(['Collapse agents', 'Needs me only', 'Filter agents · 1 active', 'Sort agents · Manual', 'Toggle color scheme', 'Missions', 'Usage'])
} }
export const QuietIconsLight: Story = { ...QuietIcons, args: { scheme: 'light' } }

/** Hover names Resources in the kit tooltip; Tab from Agent lands on Resources with the focus ring and the same tooltip; Enter opens its dialog; ArrowDown opens the conversation menu; a toggle press keeps its fill. */
export const QuietIconsFlow: Story = { play: async ({ args, canvasElement }) => {
  const scope = within(header(canvasElement))
  const resources = scope.getByRole('button', { name: 'Resources' })
  // The pointer crosses the header on its way to a control; the page's very first hover opens no tooltip.
  await userEvent.hover(header(canvasElement))
  await userEvent.hover(resources)
  await waitFor(() => expect(within(document.body).getByRole('tooltip')).toHaveTextContent('Resources'))
  await userEvent.unhover(resources)
  await waitFor(() => expect(within(document.body).queryByRole('tooltip')).toBeNull())
  scope.getByRole('button', { name: 'Actions for fractal-web' }).focus()
  await userEvent.tab()
  await expect(resources).toHaveFocus()
  await expect(resources).toHaveAttribute('data-focus-visible', 'true')
  await waitFor(() => expect(getComputedStyle(resources).outlineStyle).toBe('solid'))
  await waitFor(() => expect(within(document.body).getByRole('tooltip')).toHaveTextContent('Resources'))
  await userEvent.keyboard('{Escape}')
  // The positive twin of DisabledReason: the same wiring presses and opens the dialog when enabled.
  await userEvent.keyboard('{Enter}')
  await waitFor(() => expect(within(document.body).getByRole('dialog', { name: 'Agent resources' })).toBeVisible())
  await expect(args.onResources).toHaveBeenCalledTimes(1)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(within(document.body).queryByRole('dialog')).toBeNull())
  // The positive twin of DisabledMenu: the same MenuTrigger opens on ArrowDown when the control is enabled.
  const conversation = scope.getByRole('button', { name: 'Conversation menu' })
  conversation.focus()
  await userEvent.keyboard('{ArrowDown}')
  // MenuTrigger labels the menu by its trigger, so the menu is named "Conversation menu".
  await waitFor(() => expect(within(document.body).getByRole('menu', { name: 'Conversation menu' })).toBeVisible())
  await expect(conversation).toHaveAttribute('aria-expanded', 'true')
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(within(document.body).queryByRole('menu')).toBeNull())
  const terminal = scope.getByRole('button', { name: 'Toggle terminal drawer' })
  await userEvent.click(terminal)
  await expect(terminal).toHaveAttribute('aria-pressed', 'true')
} }

const resourcesReason = 'Read access to agent resources has not been granted.'

/**
 * Negative control: Resources with a reason is wired to the same spy and dialog, yet click, Enter and Space
 * neither press it nor open the dialog. It stays in the tab order, so Tab from Agent reaches it, and focus shows
 * "Resources · reason" in the kit tooltip; no native title anywhere.
 */
export const DisabledReason: Story = { args: { resourcesDisabledReason: resourcesReason }, play: async ({ args, canvasElement }) => {
  const scope = within(header(canvasElement))
  const resources = scope.getByRole('button', { name: 'Resources' })
  await expect(resources).toHaveAttribute('aria-disabled', 'true')
  await expect(resources).toHaveAccessibleDescription(resourcesReason)
  await expect(resources.getBoundingClientRect().height).toBe(28)
  await expect(canvasElement.querySelector('[title]')).toBeNull()
  await userEvent.click(resources)
  scope.getByRole('button', { name: 'Actions for fractal-web' }).focus()
  await userEvent.tab()
  await expect(resources).toHaveFocus()
  await waitFor(() => expect(getComputedStyle(resources).outlineStyle).toBe('solid'))
  await waitFor(() => expect(within(document.body).getByRole('tooltip')).toHaveTextContent(`Resources · ${resourcesReason}`))
  await userEvent.keyboard('{Enter}')
  await userEvent.keyboard(' ')
  await expect(args.onResources).not.toHaveBeenCalled()
  await expect(within(document.body).queryByRole('dialog')).toBeNull()
  await expect(resources).toHaveFocus()
  await userEvent.tab()
  await expect(scope.getByRole('button', { name: 'Conversation menu' })).toHaveFocus()
} }
export const DisabledReasonLight: Story = { ...DisabledReason, args: { ...DisabledReason.args, scheme: 'light' } }

const conversationReason = 'Conversation settings load once this agent is reachable.'

/**
 * Negative control: Conversation menu with a reason sits in the same MenuTrigger, whose own key handler opens on
 * Enter, Space and the arrows. Click and ArrowDown, ArrowUp, Enter and Space open no menu; Tab from Resources
 * still reaches it, and focus shows "Conversation menu · reason" in the kit tooltip.
 */
export const DisabledMenu: Story = { args: { conversationDisabledReason: conversationReason }, play: async ({ canvasElement }) => {
  const scope = within(header(canvasElement))
  const conversation = scope.getByRole('button', { name: 'Conversation menu' })
  await expect(conversation).toHaveAttribute('aria-disabled', 'true')
  await expect(conversation).toHaveAccessibleDescription(conversationReason)
  await expect(canvasElement.querySelector('[title]')).toBeNull()
  await userEvent.click(conversation)
  await expect(within(document.body).queryByRole('menu')).toBeNull()
  scope.getByRole('button', { name: 'Resources' }).focus()
  await userEvent.tab()
  await expect(conversation).toHaveFocus()
  await waitFor(() => expect(getComputedStyle(conversation).outlineStyle).toBe('solid'))
  await waitFor(() => expect(within(document.body).getByRole('tooltip')).toHaveTextContent(`Conversation menu · ${conversationReason}`))
  for (const key of ['{ArrowDown}', '{ArrowUp}', '{Enter}', ' ']) {
    await userEvent.keyboard(key)
    await expect(within(document.body).queryByRole('menu')).toBeNull()
    await expect(conversation).toHaveFocus()
  }
  await expect(conversation).toHaveAttribute('aria-expanded', 'false')
} }
export const DisabledMenuLight: Story = { ...DisabledMenu, args: { ...DisabledMenu.args, scheme: 'light' } }

const styles = stylex.create({
  root: { display: 'flex', flexDirection: { default: 'column', '@media (min-width: 640px)': 'row' }, minHeight: '100vh', boxSizing: 'border-box', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  sidebar: { display: 'flex', flexDirection: 'column', justifyContent: 'space-between', flexShrink: 0, width: { default: 'auto', '@media (min-width: 640px)': g.sidebarDefault }, backgroundColor: surface.sidebar },
  band: { height: g.footer, display: 'flex', alignItems: 'center', gap: s.xs, paddingInline: s.lg },
  push: { flexGrow: 1 },
  workspace: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 },
  actions: { display: 'flex', alignItems: 'center', gap: s.xs },
  modal: { position: 'fixed', inset: 0, display: 'grid', placeItems: 'center' },
  dialog: { padding: s.lg, backgroundColor: surface.raised, color: ink.fg, outlineStyle: 'none' },
  popover: { backgroundColor: surface.raised, color: ink.fg, padding: s.xs },
  menu: { outlineStyle: 'none' },
})

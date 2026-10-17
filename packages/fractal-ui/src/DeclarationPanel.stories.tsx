import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { DeclarationPanel, type DeclarationPanelState } from './assistant-ui/declaration/DeclarationPanel'
import { agentDeclarationKdl, longDeclarationKdl, missionDeclarationJson, missionDeclarationKdl } from './assistant-ui/declaration/declaration-fixtures'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

interface PanelArgs { readonly scheme: Scheme; readonly title: string; readonly state: DeclarationPanelState }

function Frame({ scheme, title, state }: PanelArgs) {
  return <main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <div data-testid="declaration-frame" {...stylex.props(styles.frame)}><DeclarationPanel title={title} state={state} /></div>
  </ThemePortal></main>
}

function SchemesStory({ title, state }: PanelArgs) {
  return <main {...stylex.props(styles.schemesRoot, ...baselineTheme)}>{(['dark', 'light'] as const).map(scheme => <section key={scheme} aria-label={`${scheme} declaration`} {...stylex.props(styles.schemeColumn, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <DeclarationPanel title={title} state={state} />
  </ThemePortal></section>)}</main>
}

type Play = NonNullable<StoryObj<PanelArgs>['play']>
const panelIn = (canvas: HTMLElement) => {
  const panel = canvas.querySelector<HTMLElement>('[data-testid="declaration-panel"]')
  if (panel === null) throw new Error('The declaration panel did not render')
  return panel
}
const tokens = (panel: HTMLElement, kind: string) => [...panel.querySelectorAll<HTMLElement>(`[data-token="${kind}"]`)].map(node => node.textContent)
/** Ready panels: one numbered row per source line, verbatim text, a copy action and the provenance line. */
const readyPlay = (text: string): Play => async ({ canvasElement }) => {
  const panel = panelIn(canvasElement)
  const rows = panel.querySelectorAll('[data-line]')
  const expected = text.replace(/\n$/, '').split('\n')
  if (rows.length !== expected.length) throw new Error(`Expected ${expected.length} numbered lines, got ${rows.length}`)
  const numbers = [...panel.querySelectorAll('[data-line-number]')]
  if (numbers.some(node => node.getAttribute('aria-hidden') !== 'true')) throw new Error('Line numbers must stay out of the accessible text')
  if (numbers.at(-1)?.textContent !== String(expected.length)) throw new Error('The gutter must number every line')
  const shown = [...panel.querySelectorAll('[data-line] code')].map(node => node.textContent).join('\n')
  if (shown !== expected.join('\n')) throw new Error('The code view must render the source verbatim')
  if (panel.querySelector('button[aria-label="Copy all declaration source"]') === null) throw new Error('Ready panels offer copy-all')
  if (panel.querySelector('[role="group"][tabindex="0"]') === null) throw new Error('The code group must be keyboard scrollable')
}

const meta = {
  title: 'Fractal UI/Declaration Panel',
  render: Frame,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Read-only canonical declaration for one agent or mission: KDL tinted with kit tokens, sticky line numbers, copy-all and an optional provenance line. Ready, loading, unavailable and failed states; a `json` language marks the labelled structured fallback used where st serves no KDL. Presentational and host-agnostic: the web app hosts it in a modal today and can host it in a tab later.' } } },
  args: { scheme: 'dark', title: 'agent/example/ui-design', state: { _tag: 'Ready', text: agentDeclarationKdl, language: 'kdl', source: 'Canonical KDL · revision rev-41d2a0c7 · environment values redacted by st' } },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } },
} satisfies Meta<PanelArgs>
export default meta
type Story = StoryObj<PanelArgs>

export const AgentKdl: Story = { play: async (context) => {
  await readyPlay(agentDeclarationKdl)(context)
  const panel = panelIn(context.canvasElement)
  if (!tokens(panel, 'node').includes('agent')) throw new Error('Node names are tinted')
  if (tokens(panel, 'redacted').length !== 3) throw new Error('Every redacted environment value keeps its own muted tint')
  if (panel.querySelector('[data-testid="declaration-language"]')?.textContent !== 'KDL') throw new Error('The language chip names KDL')
} }
export const AgentKdlLight: Story = { args: { scheme: 'light' }, play: readyPlay(agentDeclarationKdl) }
export const MissionKdl: Story = {
  args: { title: 'mission/example/ui-design/kdl-view', state: { _tag: 'Ready', text: missionDeclarationKdl, language: 'kdl', source: 'Authored mission KDL' } },
  play: async (context) => {
    await readyPlay(missionDeclarationKdl)(context)
    const panel = panelIn(context.canvasElement)
    if (!tokens(panel, 'property').includes('state')) throw new Error('Property names are tinted')
    if (!tokens(panel, 'number').includes('3')) throw new Error('Numbers are tinted')
  },
}
export const MissionStructuredFallback: Story = {
  args: { title: 'mission/example/ui-design/kdl-view', state: { _tag: 'Ready', text: missionDeclarationJson, language: 'json', source: 'Structured definition · revision rev-7f3c9e1a · st serves no mission KDL; environment values redacted' } },
  play: async (context) => {
    await readyPlay(missionDeclarationJson)(context)
    const panel = panelIn(context.canvasElement)
    if (panel.querySelector('[data-testid="declaration-language"]')?.textContent !== 'JSON') throw new Error('A structured fallback must not be labelled KDL')
    if (!tokens(panel, 'property').includes('"goals"')) throw new Error('JSON keys are tinted')
  },
}
export const Loading: Story = {
  args: { state: { _tag: 'Loading' } },
  play: async ({ canvasElement }) => {
    const panel = panelIn(canvasElement)
    if (panel.getAttribute('aria-busy') !== 'true' || panel.querySelector('[role="status"]')?.textContent !== 'Loading declaration…') throw new Error('Loading is announced')
    if (panel.querySelector('button') !== null) throw new Error('Nothing to copy while loading')
  },
}
export const Unavailable: Story = {
  args: { state: { _tag: 'Unavailable', message: 'Read access to declarations has not been granted to this connection.' } },
  play: async ({ canvasElement }) => {
    if (!panelIn(canvasElement).textContent?.includes('has not been granted')) throw new Error('The unavailable reason is shown')
  },
}
export const Failed: Story = {
  args: { state: { _tag: 'Failed', message: 'The declaration could not be loaded: managed agent declaration not found. Nothing was changed; close and try again.' } },
  play: async ({ canvasElement }) => {
    if (panelIn(canvasElement).querySelector('[role="alert"]') === null) throw new Error('A failed read is announced as an alert')
  },
}
export const LongFile: Story = {
  args: { title: 'mission/example/fleet-sweep', state: { _tag: 'Ready', text: longDeclarationKdl, language: 'kdl', source: 'Authored mission KDL' } },
  play: async (context) => {
    await readyPlay(longDeclarationKdl)(context)
    const panel = panelIn(context.canvasElement)
    const region = panel.querySelector<HTMLElement>('[role="group"]')!
    if (region.scrollHeight <= region.clientHeight) throw new Error('A long file scrolls inside the code group, not the page')
    if (panelIn(context.canvasElement).getBoundingClientRect().bottom > window.innerHeight + 1) throw new Error('The panel must stay within the viewport')
    region.scrollLeft = region.scrollWidth
    const first = region.querySelector<HTMLElement>('[data-line-number]')!
    if (Math.abs(first.getBoundingClientRect().left - region.getBoundingClientRect().left) > 2) throw new Error('Line numbers stay pinned while scrolling sideways')
  },
}
export const Schemes: Story = { render: SchemesStory, play: async ({ canvasElement, args }) => {
  const panels = canvasElement.querySelectorAll<HTMLElement>('[data-testid="declaration-panel"]')
  if (panels.length !== 2) throw new Error('Both schemes render side by side')
  for (const panel of panels) {
    const code = panel.querySelector<HTMLElement>('[data-testid="declaration-code"]')
    if (code?.getAttribute('role') !== 'group' || code.tabIndex !== 0 || code.getAttribute('aria-label') !== `${args.title} declaration source`) throw new Error('Each repeated source must remain a named keyboard-scrollable group, not a duplicate landmark')
  }
} }

const styles = stylex.create({
  root: { height: '100vh', boxSizing: 'border-box', padding: s.md, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  frame: { height: '100%', maxWidth: g.lane, marginInline: 'auto', backgroundColor: surface.raised, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, overflow: 'hidden' },
  schemesRoot: { height: '100vh', boxSizing: 'border-box', display: 'grid', gridTemplateColumns: { default: 'minmax(0, 1fr)', '@media (min-width: 48rem)': 'repeat(2, minmax(0, 1fr))' }, gridAutoRows: 'minmax(0, 1fr)', gap: s.md, padding: s.md, backgroundColor: surface.canvas },
  schemeColumn: { minWidth: 0, minHeight: 0, backgroundColor: surface.raised, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, overflow: 'hidden' },
})

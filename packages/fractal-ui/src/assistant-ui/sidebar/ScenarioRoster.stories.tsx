import type { Args, Meta, StoryObj } from '@storybook/react-vite'
import { useScenarioSlice } from '@smalltalk/st3-scenarios/react'
import { scenarioArgTypes } from '@smalltalk/st3-scenarios/storybook'
import * as stylex from '@stylexjs/stylex'
import { SidebarAgentRow } from './SidebarAgentRow'
import { projectAgentRow, projectAttention, projectRoster } from '../scenario-projections'
import { ScenarioPresentation, scenarioTime } from '../scenario-presentation'
import { borderVars, spaceVars, surfaceVars, textVars } from '../composition-tokens.stylex'

function ScenarioRoster({ scheme, now }: { readonly scheme: 'light' | 'dark'; readonly now: number }) {
  const roster = useScenarioSlice('roster', projectRoster)
  const attention = useScenarioSlice('attention', projectAttention)
  return <ScenarioPresentation scheme={scheme} title="Agent roster">
    <div {...stylex.props(styles.roster)}>
      {roster.loading ? <p role="status">Loading agents</p> : roster.agents.length === 0 ? <p>No agents in this world</p> : roster.agents.map(agent => <SidebarAgentRow key={agent.id} item={projectAgentRow(agent, attention.cards)} now={now} />)}
    </div>
    <h2>Attention</h2>
    {attention.loading ? <p role="status">Loading attention</p> : attention.cards.length === 0 ? <p>No attention requests</p> : <ul>{attention.cards.map(card => <li key={card.id}>{card.title} · {card.state}</li>)}</ul>}
  </ScenarioPresentation>
}

const meta = {
  title: 'Fractal UI/Scenarios/Sidebar roster',
  component: SidebarAgentRow,
  args: { scheme: 'light', roster: 'default', attention: 'default' },
  parameters: { scenario: { slices: ['roster', 'attention'] } },
  argTypes: { ...scenarioArgTypes(['roster', 'attention']), scheme: { control: 'radio', options: ['light', 'dark'] } },
  render: (args, context) => <ScenarioRoster scheme={args.scheme === 'dark' ? 'dark' : 'light'} now={scenarioTime(context).now} />,
} satisfies Meta<Args>
export default meta
export const Explore = {} satisfies StoryObj<typeof meta>
export const Pinned = { parameters: { scenario: { world: 'fleet-mid-refactor' } } } satisfies StoryObj<typeof meta>

const styles = stylex.create({
  roster: { borderWidth: 1, borderStyle: 'solid', borderColor: borderVars.border, backgroundColor: surfaceVars.sidebar, color: textVars.fg, padding: spaceVars.md },
})

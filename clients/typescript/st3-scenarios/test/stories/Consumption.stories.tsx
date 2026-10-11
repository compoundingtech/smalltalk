import type { Args, Meta, StoryObj } from '@storybook/react'

import { useScenarioSlice } from '../../src/react/index.ts'

const Consumption = () => {
  const roster = useScenarioSlice('roster')
  const attention = useScenarioSlice('attention')
  return <main>
    {roster.loading ? <p>Loading roster</p> : <ul>{roster.state.agents.map((agent) => <li key={agent.id}>{agent.name}</li>)}</ul>}
    {attention.loading ? <p>Loading attention</p> : <>
      <section>{attention.state.attention.map((card) => <h2 key={card.id}>{card.title}</h2>)}</section>
      <section>{attention.state.messages.map((message) => <h3 key={message.id}>{message.title}</h3>)}</section>
    </>}
  </main>
}

const meta = {
  title: 'Scenarios/Consumption',
  component: Consumption,
  parameters: { scenario: { slices: ['roster', 'attention'] } },
} satisfies Meta<Args>

export default meta
export const Good = {} satisfies StoryObj<typeof meta>
export const Pinned = {
  parameters: { scenario: { world: 'fleet-mid-refactor' } },
} satisfies StoryObj<typeof meta>
export const Fixed = {
  parameters: { scenario: { slices: ['roster'] } },
  render: () => <p>Fixed presentation without scenario data</p>,
} satisfies StoryObj<typeof meta>

const UndeclaredConsumption = () => {
  const agents = useScenarioSlice('roster')
  useScenarioSlice('details')
  return agents.loading ? <p>Loading roster</p> : <ul>{agents.state.agents.map((agent) => <li key={agent.id}>{agent.name}</li>)}</ul>
}

export const Undeclared = {
  parameters: { scenario: { slices: ['roster'] } },
  render: () => <UndeclaredConsumption />,
} satisfies StoryObj<typeof meta>

const SyncConsumption = () => {
  const sync = useScenarioSlice('sync')
  return sync.loading ? <p>Loading sync</p> : <p>{Object.entries(sync.status).map(([surface, status]) => `${surface}: ${JSON.stringify(status)}`).join(', ')}</p>
}

export const Sync = {
  parameters: { scenario: { slices: ['sync'] } },
  render: () => <SyncConsumption />,
} satisfies StoryObj<typeof meta>

const InvariantConsumption = () => {
  useScenarioSlice('sync')
  return <p>Layout intentionally independent of sync status</p>
}

export const Invariant = {
  parameters: { scenario: { slices: ['sync'], invariant: { sync: 'This layout does not display transport status' } } },
  render: () => <InvariantConsumption />,
} satisfies StoryObj<typeof meta>

const LoadingRosterConsumption = () => {
  const roster = useScenarioSlice('roster')
  return roster.loading ? <p>Loading roster</p> : <ul>{roster.state.agents.map((agent) => <li key={agent.id}>{agent.name}</li>)}</ul>
}

export const LoadingRoster = {
  parameters: { scenario: { slices: ['roster'] } },
  render: () => <LoadingRosterConsumption />,
} satisfies StoryObj<typeof meta>

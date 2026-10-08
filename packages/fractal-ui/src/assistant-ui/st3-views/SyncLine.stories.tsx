import type { Args, Meta, StoryObj } from '@storybook/react-vite'
import { useScenarioSlice } from '@smalltalk/st3-scenarios/react'
import { scenarioArgTypes } from '@smalltalk/st3-scenarios/storybook'
import { SyncLine } from './SyncLine'
import { projectSync } from '../scenario-projections'
import { ScenarioPresentation, scenarioTime } from '../scenario-presentation'

function ScenarioSync({ scheme, anchor, now }: { readonly scheme: 'light' | 'dark'; readonly anchor: number; readonly now: number }) {
  const lines = useScenarioSlice('sync', slice => projectSync(slice, anchor, now))
  return <ScenarioPresentation scheme={scheme} title="Sync observations">
    {lines.map(line => <section key={line.surface}>
      <h2>{line.surface}</h2>
      {line.status === undefined ? <p>Sync observation unavailable</p> : <SyncLine status={line.status} label={line.surface} now={now} observedAt={line.observedAt} socket gateway="Scenario gateway" />}
    </section>)}
  </ScenarioPresentation>
}

const meta = {
  title: 'Fractal UI/Scenarios/Sync line',
  component: SyncLine,
  args: { scheme: 'light', sync: 'default' },
  parameters: { scenario: { slices: ['sync'] } },
  argTypes: { ...scenarioArgTypes(['sync']), scheme: { control: 'radio', options: ['light', 'dark'] } },
  render: (args, context) => <ScenarioSync scheme={args.scheme === 'dark' ? 'dark' : 'light'} {...scenarioTime(context)} />,
} satisfies Meta<Args>
export default meta
export const Explore = {} satisfies StoryObj<typeof meta>
export const Pinned = { parameters: { scenario: { world: 'fleet-mid-refactor' } } } satisfies StoryObj<typeof meta>
export const SocketDropped = { args: { sync: 'socket-dropped' } } satisfies StoryObj<typeof meta>
export const Reconnected = { args: { sync: 'reconnected' } } satisfies StoryObj<typeof meta>
export const SubscriptionLimitLocal = { args: { sync: 'subscription-limit-local' } } satisfies StoryObj<typeof meta>

import type { Args, Meta, StoryObj } from '@storybook/react-vite'
import { useScenarioSlice } from '@smalltalk/st3-scenarios/react'
import { scenarioArgTypes } from '@smalltalk/st3-scenarios/storybook'
import { EmbraceThread } from './EmbraceThread'
import { EmbraceRuntimeProvider } from './EmbraceRuntime'
import { projectConversation } from './scenario-projections'
import { ScenarioPresentation } from './scenario-presentation'

function ScenarioConversation({ scheme }: { readonly scheme: 'light' | 'dark' }) {
  const conversation = useScenarioSlice('conversation', projectConversation)
  return <ScenarioPresentation scheme={scheme} title="Conversation threads">
    {conversation.loading ? <p role="status">Loading conversations</p> : conversation.threads.length === 0 ? <p>No conversation history</p> : conversation.threads.map(thread => <section key={thread.agent}>
      <h2>{thread.agent}</h2>
      {thread.unprojected.length > 0 && <p>Not yet projected: {thread.unprojected.join(', ')}</p>}
      <EmbraceRuntimeProvider options={{
        messages: thread.items,
        isRunning: thread.items.some(item => item._tag === 'Text' && item.streaming),
        onNew: async () => { throw new Error('Scenario conversations are read-only') },
      }}>
        <EmbraceThread items={thread.items} composer={false} embrace="E3" />
      </EmbraceRuntimeProvider>
    </section>)}
  </ScenarioPresentation>
}

const meta = {
  title: 'Fractal UI/Scenarios/Conversation thread',
  component: EmbraceThread,
  args: { scheme: 'light', conversation: 'default' },
  parameters: {
    scenario: { slices: ['conversation'] },
    docs: { description: { story: 'Uses the existing workshop fold for message, content, tool_call and tool_result. Status, usage, error, redaction, truncation and future wire kinds are not yet projected; present unsupported kinds are listed beside the actual thread.' } },
  },
  argTypes: { ...scenarioArgTypes(['conversation']), scheme: { control: 'radio', options: ['light', 'dark'] } },
  render: args => <ScenarioConversation scheme={args.scheme === 'dark' ? 'dark' : 'light'} />,
} satisfies Meta<Args>
export default meta
export const Explore = {} satisfies StoryObj<Args>
export const Pinned = { parameters: { scenario: { world: 'fleet-mid-refactor' } } } satisfies StoryObj<Args>

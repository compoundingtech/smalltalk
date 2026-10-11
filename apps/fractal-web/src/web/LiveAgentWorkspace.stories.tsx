import type { ComponentProps } from 'react'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { waitFor } from 'storybook/test'
import { LiveAgentWorkspace } from './LiveAgentWorkspace.tsx'
import { AppStoryStates, prepareAppStory, type AppStoryArgs } from '../fixtures/AppStoryHarness.tsx'
import { assertProductionAppStates } from '../fixtures/appStoryAssertions.ts'

const meta = {
  title: 'Fractal/App/LiveAgentWorkspace',
  component: LiveAgentWorkspace,
  parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', width: 1440 },
  beforeEach: ({ args }) => prepareAppStory(args.scheme),
  render: args => <AppStoryStates {...args} renderSurface={fixture => <LiveAgentWorkspace />} />,
  play: async ({ canvasElement }) => {
    await waitFor(() => assertProductionAppStates(canvasElement, true), { timeout: 15000 })
    canvasElement.dataset.appPlay = 'passed'
  },
} satisfies Meta<ComponentProps<typeof LiveAgentWorkspace> & AppStoryArgs>
export default meta
type Story = StoryObj<typeof meta>

export const AllStates: Story = {}
export const AllStatesLight: Story = { args: { scheme: 'light' } }
export const AllStates600: Story = { args: { width: 600 } }
export const AllStatesLight600: Story = { args: { scheme: 'light', width: 600 } }

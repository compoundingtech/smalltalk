import type { Meta, StoryObj } from '@storybook/react-vite'
import { ContentNetworkStory, sources, deferredStory, allowedStory, hostOpensStory } from './ContentNetwork.fixtures.tsx'

const meta = { title: 'Fractal/Kit/Content network safety', component: ContentNetworkStory, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof ContentNetworkStory>
export default meta
type Story = StoryObj<typeof meta>

export const TranscriptRemote: Story = deferredStory('Transcript', sources.Remote)
export const TranscriptLoopback: Story = deferredStory('Transcript', sources.Loopback)
export const TranscriptMetadata: Story = deferredStory('Transcript', sources.Metadata)
export const TranscriptData: Story = deferredStory('Transcript', sources.Data)
export const TranscriptLinked: Story = deferredStory('Transcript', sources.Linked)
export const TranscriptAllowedAttachment: Story = allowedStory('Transcript')
export const TranscriptHostOpensRemote: Story = hostOpensStory('Transcript', sources.Remote)
export const TranscriptHostOpensLoopback: Story = hostOpensStory('Transcript', sources.Loopback)

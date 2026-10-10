import type { Meta, StoryObj } from '@storybook/react-vite'
import { ContentNetworkStory, sources, deferredStory, allowedStory, hostOpensStory } from './ContentNetwork.fixtures.tsx'

const meta = { title: 'Fractal/Explore/Content network safety', component: ContentNetworkStory, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof ContentNetworkStory>
export default meta
type Story = StoryObj<typeof meta>

export const EmbraceRemote: Story = deferredStory('Embrace', sources.Remote)
export const EmbraceLoopback: Story = deferredStory('Embrace', sources.Loopback)
export const EmbraceMetadata: Story = deferredStory('Embrace', sources.Metadata)
export const EmbraceData: Story = deferredStory('Embrace', sources.Data)
export const EmbraceLinked: Story = deferredStory('Embrace', sources.Linked)
export const EmbraceAllowedAttachment: Story = allowedStory('Embrace')
export const EmbraceHostOpensRemote: Story = hostOpensStory('Embrace', sources.Remote)
export const EmbraceHostOpensMetadata: Story = hostOpensStory('Embrace', sources.Metadata)
export const ToolPreviewRemote: Story = deferredStory('ToolPreview', sources.Remote)
export const ToolPreviewLoopback: Story = deferredStory('ToolPreview', sources.Loopback)
export const ToolPreviewMetadata: Story = deferredStory('ToolPreview', sources.Metadata)
export const ToolPreviewData: Story = deferredStory('ToolPreview', sources.Data)
export const ToolPreviewLinked: Story = deferredStory('ToolPreview', sources.Linked)
export const ToolPreviewAllowedAttachment: Story = allowedStory('ToolPreview')
export const ToolPreviewHostOpensRemote: Story = hostOpensStory('ToolPreview', sources.Remote)
export const ToolPreviewHostOpensLinked: Story = hostOpensStory('ToolPreview', sources.Linked)

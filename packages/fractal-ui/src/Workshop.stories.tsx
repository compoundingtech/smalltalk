import type { Meta, StoryObj } from '@storybook/react-vite'
import { useArgs } from 'storybook/preview-api'
import { Workshop } from './Workshop'
import type { Theme } from './kit'

const meta = {
  title: 'Fractal UI/Visual language',
  component: Workshop,
  args: { direction: 'folio', scheme: 'light', density: 'comfortable' },
  argTypes: {
    direction: { control: 'radio', options: ['folio', 'relay', 'orbit'] },
    scheme: { control: 'radio', options: ['light', 'dark'] },
    density: { control: 'radio', options: ['compact', 'comfortable'] },
    onThemeChange: { table: { disable: true } },
  },
  render: function Render(args: Theme & { onThemeChange?: (change: Partial<Theme>) => void }) {
    const [, updateArgs] = useArgs()
    return <Workshop {...args} onThemeChange={(change: Partial<Theme>) => updateArgs(change)} />
  },
} satisfies Meta<typeof Workshop>
export default meta
type Story = StoryObj<typeof meta>
export const Explore: Story = {}
export const FolioLight: Story = { args: { direction: 'folio', scheme: 'light' } }
export const FolioDark: Story = { args: { direction: 'folio', scheme: 'dark' } }
export const RelayLight: Story = { args: { direction: 'relay', scheme: 'light' } }
export const RelayDark: Story = { args: { direction: 'relay', scheme: 'dark' } }
export const OrbitLight: Story = { args: { direction: 'orbit', scheme: 'light' } }
export const OrbitDark: Story = { args: { direction: 'orbit', scheme: 'dark' } }
export const Compact: Story = { args: { density: 'compact' } }

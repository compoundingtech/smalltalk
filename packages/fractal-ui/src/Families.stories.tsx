import type { Meta, StoryObj } from '@storybook/react-vite'
import { useArgs } from 'storybook/preview-api'
import { Families } from './Families'
import type { Theme } from './kit'

const meta = {
  title: 'Fractal UI/Component families',
  component: Families,
  args: { direction: 'folio', scheme: 'light', density: 'comfortable' },
  argTypes: {
    direction: { control: 'radio', options: ['folio', 'relay', 'orbit'] },
    scheme: { control: 'radio', options: ['light', 'dark'] },
    density: { control: 'radio', options: ['compact', 'comfortable'] },
    onThemeChange: { table: { disable: true } },
  },
  render: function Render(args: Theme & { onThemeChange?: (change: Partial<Theme>) => void }) {
    const [, updateArgs] = useArgs()
    return <Families {...args} onThemeChange={(change: Partial<Theme>) => updateArgs(change)} />
  },
} satisfies Meta<typeof Families>
export default meta
type Story = StoryObj<typeof meta>
export const AllFamilies: Story = {}
export const FamiliesDark: Story = { args: { scheme: 'dark' } }

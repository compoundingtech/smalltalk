import type { Meta, StoryObj } from '@storybook/react'
import { expect, within } from 'storybook/test'
import { NativeTurnFooter } from './NativeTurnFooter.tsx'

const meta = { title: 'Fractal Web/Transcript/Native facts', component: NativeTurnFooter, args: { facts: { model: 'example-model', tokens: 4200, costUsd: 0.025 } } } satisfies Meta<typeof NativeTurnFooter>
export default meta
type Story = StoryObj<typeof meta>
export const AllStates: Story = {
  render: () => <section><NativeTurnFooter facts={{ model: 'example-model', tokens: 4200, costUsd: 0.025 }} /><NativeTurnFooter facts={{ model: 'example-model', tokens: 0, costUsd: 0 }} /><div data-testid="missing-control"><NativeTurnFooter facts={{}} /></div></section>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await expect(canvas.getAllByTestId('native-turn-footer')).toHaveLength(2)
    await expect(canvas.getByText('example-model · 4,200 tokens · $0.0250')).toBeVisible()
    await expect(canvas.getByText('example-model · 0 tokens · $0.0000')).toBeVisible()
    // Failing control: missing facts must never synthesize model, tokens or a cost.
    await expect(within(canvas.getByTestId('missing-control')).queryByTestId('native-turn-footer')).toBeNull()
  },
}

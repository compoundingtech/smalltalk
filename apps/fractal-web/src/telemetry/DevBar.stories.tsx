import type { Meta, StoryObj } from '@storybook/react'

import { DevBar } from './DevBar.tsx'

const meta = {
  title: 'wf/Monitor/Developer Bar',
  component: DevBar,
  parameters: { devBar: false },
  args: { defaultVisible: true },
  // This story owns the real bar; the preview suppresses its singleton to avoid two engines.
  render: (args) => (
    <main>
      <h1>Developer transport bar</h1>
      <p>
        One 24px row for live performance meters and the actual st3 SDK HTTP + collections WebSocket
        transport, with horizontal scrolling on narrow screens. Hover to preview provenance and
        measurement details; click the wf button to pin them. Mod Shift B toggles visibility.
      </p>
      <p>
        Requests in flight, p50/p95 HTTP response-header latency over the latest 256 requests, HTTP
        errors, shared socket subscriptions and incoming messages per second. The row also shows the
        shared display version, including dirty-source semantics; expanded details retain the full
        source revision and deployment identity. FPS, Effect fibers, render commits, decode time,
        retained entries, terminal updates, socket errors, resyncs and retries stay visible without
        hover.
      </p>
      <DevBar {...args} />
    </main>
  ),
} satisfies Meta<typeof DevBar>
export default meta

type Story = StoryObj<typeof meta>
export const Compact: Story = {}
export const Expanded: Story = {
  play: async ({ canvasElement }) => {
    const button = canvasElement.querySelector<HTMLButtonElement>(
      '[aria-controls="wf-devbar-details"]',
    )
    if (button === null) throw new Error('Developer details control did not mount')
    button.click()
  },
}
export const AllStates: Story = {
  render: (args) => (
    <main>
      <h1>Developer bar states</h1>
      <p>
        A single live instance keeps measurement ownership unambiguous. Hover to preview the details
        or use the wf button to pin and collapse them. Mod Shift B hides and restores the bar; SDK
        transport values come from actual measurements, not story fixtures.
      </p>
      <DevBar {...args} />
    </main>
  ),
}

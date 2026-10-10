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
        The composable developer shell shows live frame, long-frame and approximate JS-heap meters
        alongside the actual st3 SDK HTTP and collections WebSocket transport. Counters and Meters
        open independent panels. Mod Backquote toggles the selected panel; Mod Shift B toggles visibility.
      </p>
      <p>
        Requests in flight, p50/p95 HTTP response-header latency over the latest 256 requests, HTTP
        errors, shared socket subscriptions and incoming messages per second. The row also shows the
        shared display version, including dirty-source semantics; expanded details retain the full
        source revision and deployment identity. The Counters panel retains sorted app measurements,
        including render commits, decode time, retained entries, socket errors, resyncs and retries.
        Meter strips share one collector session and can be frozen independently.
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
      'button[aria-expanded="false"]:not([aria-label])',
    )
    if (button === null) throw new Error('Counters panel control did not mount')
    button.click()
  },
}
export const AllStates: Story = {
  render: (args) => (
    <main>
      <h1>Developer bar states</h1>
      <p>
        A single live instance keeps measurement ownership unambiguous. Open Counters or Meters,
        use Mod Backquote to toggle a panel and Escape to close it, or freeze a strip to inspect history.
        Mod Shift B hides and restores the bar; transport values come from actual measurements.
      </p>
      <DevBar {...args} />
    </main>
  ),
}

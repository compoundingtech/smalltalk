import { expect, userEvent } from 'storybook/test'

type Fault = 'none' | 'instant' | 'late-hide' | 'omit-input'

/** Measures real painted scroll positions and pointer intent in either production lane. */
export async function proveAffordanceMotion({ lane, pill, stream, cancel = false, fault = 'none' }: {
  readonly lane: HTMLElement
  readonly pill: HTMLButtonElement
  readonly stream: () => void
  readonly cancel?: boolean
  readonly fault?: Fault
}) {
  const nativeFrame = window.requestAnimationFrame
  // Capture the scheduling stack, not the later callback stack, for the instant-motion control.
  if (fault === 'instant') window.requestAnimationFrame = callback => {
    const animation = new Error().stack?.includes('FollowAnimation') === true
    return nativeFrame.call(window, time => callback(animation ? time + 1000 : time))
  }
  const startTop = lane.scrollTop
  const startHeight = lane.scrollHeight
  const samples: { readonly at: number; readonly progress: number; readonly top: number }[] = []
  let clickedAt: number | undefined
  let hiddenAfter = Infinity
  let grew = false
  let cancelledTop: number | undefined
  const intent = () => { clickedAt = performance.now() }
  pill.addEventListener('pointerdown', intent, { once: true, capture: true })
  const began = performance.now()
  let frame: number | undefined
  const lateHide = new MutationObserver(() => {
    if (fault === 'late-hide' && clickedAt !== undefined && performance.now() - clickedAt < 150 && pill.hidden) pill.hidden = false
  })
  if (fault === 'late-hide') lateHide.observe(pill, { attributes: true, attributeFilter: ['hidden'] })
  // ES2022 does not include Promise.withResolvers.
  const measured = new Promise<void>(resolve => {
    const sample = (time: number) => {
      if (clickedAt !== undefined) {
        const elapsed = time - clickedAt
        if (fault === 'late-hide' && elapsed >= 150) pill.hidden = true
        if (pill.hidden && hiddenAfter === Infinity) hiddenAfter = elapsed
        const end = lane.scrollHeight - lane.clientHeight
        const top = lane.scrollTop
        if (top > startTop + 1 && end - top > 1 && (samples.length === 0 || Math.abs(top - samples.at(-1)!.top) > 1)) samples.push({ at: elapsed, progress: (top - startTop) / (end - startTop), top })
        // Interrupt while the virtual live row is mounted, so streamed growth is measured too.
        if (samples.length >= 3 && !grew && (!cancel || end - top <= 20)) {
          if (cancel && fault !== 'omit-input') {
            lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -40, bubbles: true }))
            lane.scrollTop -= 40
            lane.dispatchEvent(new Event('scroll'))
            cancelledTop = lane.scrollTop
          }
          stream()
          grew = true
        }
        if (elapsed >= 350) { resolve(); return }
      } else if (time - began > 2000) { resolve(); return }
      frame = nativeFrame.call(window, sample)
    }
    frame = nativeFrame.call(window, sample)
  })
  try {
    await userEvent.click(pill)
    await measured
    await expect(hiddenAfter, 'click intent must hide the follow pill within 100ms').toBeLessThanOrEqual(100)
    await expect(samples.length, 'return to end must paint at least three intermediate scroll frames').toBeGreaterThanOrEqual(3)
    await expect(lane.scrollHeight, 'the live edge must grow during the return animation').toBeGreaterThan(startHeight)
    if (cancel) {
      await expect(lane.dataset['followState'], 'reader input must cancel the return animation').toBe('detached')
      await expect(cancelledTop, 'reader input must be delivered during motion').toBeDefined()
      await expect(Math.abs(lane.scrollTop - cancelledTop!), 'cancelled motion must not overwrite the reader position').toBeLessThanOrEqual(1)
    } else {
      const first = samples[0]!
      const second = samples[1]!
      const beforeLast = samples.at(-2)!
      const last = samples.at(-1)!
      const earlySpeed = (second.progress - first.progress) / (second.at - first.at)
      const lateSpeed = (last.progress - beforeLast.progress) / (last.at - beforeLast.at)
      await expect(earlySpeed, 'return motion must ease down rather than move at constant speed').toBeGreaterThan(lateSpeed * 1.2)
      await expect(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 'return motion must reach the growing live edge').toBeLessThanOrEqual(1)
      await expect(lane.dataset['followState']).toBe('attached')
    }
  } finally {
    if (frame !== undefined) cancelAnimationFrame(frame)
    lateHide.disconnect()
    pill.removeEventListener('pointerdown', intent, true)
    window.requestAnimationFrame = nativeFrame
  }
}

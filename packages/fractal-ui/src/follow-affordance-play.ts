import { expect, userEvent } from 'storybook/test'

type Fault = 'none' | 'instant' | 'late-hide' | 'omit-input'

export interface AffordanceMotionOptions {
  readonly cancel?: boolean
  readonly fault?: Fault
  readonly activation?: 'pointer' | 'Enter' | 'Space'
  readonly readerInput?: 'wheel' | 'key' | 'touch'
}

/** Measures real painted scroll positions and pointer intent in either production lane. */
export async function proveAffordanceMotion({ lane, pill, stream, cancel = false, fault = 'none', activation = 'pointer', readerInput = 'wheel' }: {
  readonly lane: HTMLElement
  readonly pill: HTMLButtonElement
  readonly stream: () => void
} & AffordanceMotionOptions) {
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
  const intentType = activation === 'pointer' ? 'pointerdown' : 'keydown'
  pill.addEventListener(intentType, intent, { once: true, capture: true })
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
            if (readerInput === 'wheel') lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -40, bubbles: true }))
            else if (readerInput === 'key') lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true }))
            else lane.dispatchEvent(new Event('touchmove', { bubbles: true }))
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
    if (activation === 'pointer') await userEvent.click(pill)
    else {
      pill.focus({ preventScroll: true })
      await userEvent.keyboard(activation === 'Enter' ? '{Enter}' : '[Space]')
    }
    await measured
    await expect(hiddenAfter, 'press intent must hide the follow pill within 100ms').toBeLessThanOrEqual(100)
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
    pill.removeEventListener(intentType, intent, true)
    window.requestAnimationFrame = nativeFrame
  }
}

/** This story runs under actual browser reduced-motion emulation, not a positive media-query mock. */
export async function proveReducedMotion(lane: HTMLElement, pill: HTMLButtonElement, ignorePreference = false) {
  const nativeMedia = window.matchMedia
  await expect(nativeMedia.call(window, '(prefers-reduced-motion: reduce)').matches, 'run this accessibility story with reduced motion enabled').toBe(true)
  // Negative control: deliberately consult the opposite native query, reproducing ignored preference.
  if (ignorePreference) window.matchMedia = query => nativeMedia.call(window, query === '(prefers-reduced-motion: reduce)' ? '(prefers-reduced-motion: no-preference)' : query)
  let immediateGap = Infinity
  let hideAfter = Infinity
  const intent = () => {
    const started = performance.now()
    queueMicrotask(() => {
      immediateGap = lane.scrollHeight - lane.clientHeight - lane.scrollTop
      if (pill.hidden) hideAfter = performance.now() - started
    })
  }
  pill.addEventListener('pointerdown', intent, { once: true, capture: true })
  try {
    await userEvent.click(pill)
    await expect(hideAfter, 'reduced-motion press intent must hide within 100ms').toBeLessThanOrEqual(100)
    await expect(immediateGap, 'reduced-motion return must reach the current end immediately').toBeLessThanOrEqual(1)
    for (let frame = 0; frame < 3; frame++) {
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
      await expect(lane.scrollHeight - lane.clientHeight - lane.scrollTop, 'reduced-motion return must not paint intermediate positions').toBeLessThanOrEqual(1)
    }
  } finally {
    pill.removeEventListener('pointerdown', intent, true)
    window.matchMedia = nativeMedia
  }
}

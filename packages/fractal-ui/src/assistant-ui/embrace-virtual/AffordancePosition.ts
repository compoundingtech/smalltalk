function composerNear(frame: HTMLElement) {
  for (let ancestor = frame.parentElement; ancestor !== null && ancestor !== frame.ownerDocument.body; ancestor = ancestor.parentElement) {
    const composer = ancestor.querySelector<HTMLElement>('[data-testid="kit-composer"] form,[data-follow-composer]')
    if (composer !== null) return composer
  }
}

/** Follow the actual composer surface, including a host dock's padding or overlay geometry. */
export function observeAffordancePosition(button: HTMLButtonElement) {
  const frame = button.parentElement
  if (frame === null) return
  const composer = composerNear(frame)
  if (composer === undefined) return
  const measure = () => {
    button.style.setProperty('--fractal-follow-composer-gap', `${frame.getBoundingClientRect().bottom - composer.getBoundingClientRect().top}px`)
  }
  const observer = new ResizeObserver(measure)
  observer.observe(frame)
  observer.observe(composer)
  measure()
  return () => observer.disconnect()
}

/** Activation must not leave keyboard focus on a hidden action. Pointer activation keeps draft focus. */
export function returnAffordanceFocus(button: HTMLButtonElement, viewport?: HTMLDivElement | null) {
  if (button.ownerDocument.activeElement !== button) return
  const composer = button.parentElement === null ? undefined : composerNear(button.parentElement)
  const input = composer?.querySelector<HTMLElement>('textarea,[role="textbox"],[contenteditable="true"]')
  input?.focus({ preventScroll: true })
  if (button.ownerDocument.activeElement !== button) return
  viewport?.focus({ preventScroll: true })
  if (button.ownerDocument.activeElement === button) button.blur()
}

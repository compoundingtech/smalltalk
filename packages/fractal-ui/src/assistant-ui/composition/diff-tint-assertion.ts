type Color = readonly [number, number, number, number]

/** Normalize computed CSS colors (including color-mix) through the browser's sRGB canvas. */
function color(value: string): Color {
  const context = document.createElement('canvas').getContext('2d')!
  context.clearRect(0, 0, 1, 1)
  context.fillStyle = value
  context.fillRect(0, 0, 1, 1)
  const pixel = context.getImageData(0, 0, 1, 1).data
  return [pixel[0]! / 255, pixel[1]! / 255, pixel[2]! / 255, pixel[3]! / 255]
}
function over(foreground: Color, background: Color): Color {
  const alpha = foreground[3] + background[3] * (1 - foreground[3])
  return [0, 1, 2].map(index => (foreground[index]! * foreground[3] + background[index]! * background[3] * (1 - foreground[3])) / alpha).concat(alpha) as unknown as Color
}
function luminance(rgb: Color): number {
  const linear = rgb.slice(0, 3).map(channel => channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4)
  return linear[0]! * 0.2126 + linear[1]! * 0.7152 + linear[2]! * 0.0722
}
function background(element: Element | null): Color {
  if (element === null) return [1, 1, 1, 1]
  const own = color(getComputedStyle(element).backgroundColor)
  return own[3] === 1 ? own : over(own, background(element.parentElement))
}
function hue(rgb: Color): number {
  const [red, green, blue] = rgb
  const maximum = Math.max(red, green, blue), minimum = Math.min(red, green, blue)
  const chroma = maximum - minimum
  if (chroma === 0) return NaN
  return ((maximum === red ? (green - blue) / chroma : maximum === green ? (blue - red) / chroma + 2 : (red - green) / chroma + 4) * 60 + 360) % 360
}

function declarationToken(element: Element, property: string): string | undefined {
  let token: string | undefined
  const visit = (rules: CSSRuleList) => {
    for (const rule of rules) {
      if (rule instanceof CSSStyleRule && element.matches(rule.selectorText)) {
        const value = rule.style.getPropertyValue(property)
        if (value !== '') token = value.match(/var\((--[^,)]+)/)?.[1]
      } else if (rule instanceof CSSGroupingRule) visit(rule.cssRules)
    }
  }
  for (const sheet of document.styleSheets) visit(sheet.cssRules)
  return token
}

/** Also accepts the pre-token row markup, so the same assertion serves as a negative control. */
export function assertDiffTint(canvas: HTMLElement) {
  const rows = [...canvas.querySelectorAll('code')].map(code => code.parentElement!).filter(row => row.children[0]?.textContent?.trim().endsWith('+') || row.children[0]?.textContent?.trim().endsWith('−'))
  const added = rows.find(row => row.children[0]?.textContent?.trim().endsWith('+'))
  const removed = rows.find(row => row.children[0]?.textContent?.trim().endsWith('−'))
  if (!added || !removed) throw new Error('Diff tint requires an added and a removed row')
  const addWash = color(getComputedStyle(added).backgroundColor)
  const deleteWash = color(getComputedStyle(removed).backgroundColor)
  const marker = color(getComputedStyle(added.children[0]!).borderLeftColor)
  const pane = background(added.parentElement)
  const addComposite = over(addWash, pane), deleteComposite = over(deleteWash, pane)
  const addDelta = Math.abs(luminance(addComposite) - luminance(pane))
  const deleteDelta = Math.abs(luminance(deleteComposite) - luminance(pane))
  const chroma = Math.max(...addComposite.slice(0, 3)) - Math.min(...addComposite.slice(0, 3))
  const addHue = hue(addComposite)
  if (chroma < 0.01) throw new Error('Added-row wash is neutral grey')
  if (addHue >= 90 && addHue <= 170) throw new Error('Added-row wash is green')
  const washToken = declarationToken(added, 'background-color')
  const gutterToken = declarationToken(added.children[0]!, 'border-left-color')
  if (washToken === undefined || washToken !== gutterToken) throw new Error('Added wash and gutter do not share a semantic token')
  // Canvas unpremultiplication of a 9% wash can round by up to 6/255.
  if (addWash.slice(0, 3).some((channel, index) => Math.abs(channel - marker[index]!) > 6 / 255)) throw new Error('Added wash and gutter do not share addition ink')
  if (deleteDelta === 0 || Math.abs(addDelta / deleteDelta - 1) > 0.2) throw new Error(`Addition/deletion luminance delta mismatch: ${addDelta} / ${deleteDelta}`)
  return { pane, addWash, deleteWash, marker, washToken, gutterToken, addComposite, deleteComposite, paneLuminance: luminance(pane), addLuminance: luminance(addComposite), deleteLuminance: luminance(deleteComposite), addDelta, deleteDelta, deltaRatio: addDelta / deleteDelta, chroma, hue: addHue }
}

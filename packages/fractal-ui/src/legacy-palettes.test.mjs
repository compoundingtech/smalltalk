import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'

const css = readFileSync(new URL('./tokens.css', import.meta.url), 'utf8')
const palettes = new Map()
for (const [, direction, scheme, body] of css.matchAll(/\[data-direction='(folio|relay|orbit)'\](?:\[data-scheme='(dark)'\])?\s*\{([^}]+)\}/g)) {
  const own = Object.fromEntries([...body.matchAll(/--([\w-]+):\s*([^;]+);/g)].map(([, name, value]) => [name, value.trim()]))
  const name = `${direction}.${scheme ?? 'light'}`
  assert.ok(!palettes.has(name), `Duplicate palette ${name}`)
  palettes.set(name, { ...palettes.get(`${direction}.light`), ...own })
}
function color(value) {
  const hex = /^#([\da-f]{6})$/i.exec(value)
  if (hex) return [0, 2, 4].map(offset => parseInt(hex[1].slice(offset, offset + 2), 16) / 255).concat(1)
  const rgba = /^rgba?\(([^)]+)\)$/.exec(value)
  assert.ok(rgba, `Unrecognized palette color ${value}`)
  const values = rgba[1].split(',').map(Number)
  assert.ok((values.length === 3 || values.length === 4) && values.every(Number.isFinite), `Malformed palette color ${value}`)
  return values.slice(0, 3).map(value => value / 255).concat(values[3] ?? 1)
}
function luminance(rgb) {
  const linear = rgb.slice(0, 3).map(value => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4)
  return linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
}

test('all shipped direction/scheme palettes have no green good or added value', () => {
  assert.deepEqual([...palettes.keys()].sort(), ['folio.dark', 'folio.light', 'orbit.dark', 'orbit.light', 'relay.dark', 'relay.light'])
  const findings = []
  for (const [name, palette] of palettes) for (const token of ['good', 'added']) {
    const [red, green, blue] = color(palette[token])
    const maximum = Math.max(red, green, blue), minimum = Math.min(red, green, blue), chroma = maximum - minimum
    const hue = ((maximum === red ? (green - blue) / chroma : maximum === green ? (blue - red) / chroma + 2 : (red - green) / chroma + 4) * 60 + 360) % 360
    if (hue >= 90 && hue <= 170 && chroma > 0.02) findings.push(`${name}/${token}: ${palette[token]}`)
  }
  assert.deepEqual(findings, [])
})

test('good uses the direction soft foreground and additions match deletion luminance deltas', () => {
  for (const [name, palette] of palettes) {
    assert.deepEqual(color(palette.good), color(palette.ink).slice(0, 3).concat(0.8), `${name}: good is not the 80% soft foreground`)
    const panel = luminance(color(palette.panel))
    const addedDelta = Math.abs(luminance(color(palette.added)) - panel)
    const removedDelta = Math.abs(luminance(color(palette.removed)) - panel)
    assert.ok(removedDelta > 0, `${name}: deletion wash is invisible`)
    const ratio = addedDelta / removedDelta
    assert.ok(Math.abs(ratio - 1) <= 0.2, `${name}: addition/deletion luminance ratio ${ratio}`)
  }
})

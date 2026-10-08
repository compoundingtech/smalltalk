/**
 * Seeded generator (sfc32). `fork(rng, label)` derives an independent child from a stable label,
 * so adding a draw in one place never shifts values drawn elsewhere.
 */
export interface Rng {
  readonly seed: readonly [number, number, number, number]
  /** Uniform float in [0, 1). */
  readonly next: () => number
}

/** cyrb128: hashes a string into four 32-bit words. */
const hash128 = (text: string): [number, number, number, number] => {
  let h1 = 1779033703
  let h2 = 3144134277
  let h3 = 1013904242
  let h4 = 2773480762
  for (let i = 0; i < text.length; i++) {
    const k = text.charCodeAt(i)
    h1 = h2 ^ Math.imul(h1 ^ k, 597399067)
    h2 = h3 ^ Math.imul(h2 ^ k, 2869860233)
    h3 = h4 ^ Math.imul(h3 ^ k, 951274213)
    h4 = h1 ^ Math.imul(h4 ^ k, 2716044179)
  }
  h1 = Math.imul(h3 ^ (h1 >>> 18), 597399067)
  h2 = Math.imul(h4 ^ (h2 >>> 22), 2869860233)
  h3 = Math.imul(h1 ^ (h3 >>> 17), 951274213)
  h4 = Math.imul(h2 ^ (h4 >>> 19), 2716044179)
  h1 ^= h2 ^ h3 ^ h4
  h2 ^= h1
  h3 ^= h1
  h4 ^= h1
  return [h1 >>> 0, h2 >>> 0, h3 >>> 0, h4 >>> 0]
}

const sfc32 = (seed: readonly [number, number, number, number]): Rng => {
  let [a, b, c, d] = seed
  const next = () => {
    a |= 0
    b |= 0
    c |= 0
    d |= 0
    const t = (((a + b) | 0) + d) | 0
    d = (d + 1) | 0
    a = b ^ (b >>> 9)
    b = (c + (c << 3)) | 0
    c = (c << 21) | (c >>> 11)
    c = (c + t) | 0
    return (t >>> 0) / 4294967296
  }
  for (let i = 0; i < 12; i++) next()
  return { seed, next }
}

export const rngFromSeed = (seed: number | string): Rng => sfc32(hash128(`st3-scenarios:${seed}`))

/** Child generator for `label`; depends only on the parent's seed, not on its draws. */
export const fork = (rng: Rng, label: string): Rng => sfc32(hash128(`${rng.seed.join(',')}/${label}`))

/** Integer in [min, max]. */
export const int = (rng: Rng, min: number, max: number): number => min + Math.floor(rng.next() * (max - min + 1))

export const pick = <A>(rng: Rng, items: readonly A[]): A => {
  if (items.length === 0) throw new Error('pick from an empty list')
  return items[int(rng, 0, items.length - 1)]!
}

export const chance = (rng: Rng, p: number): boolean => rng.next() < p

/** `count` distinct items in a stable random order. */
export const sample = <A>(rng: Rng, items: readonly A[], count: number): A[] => {
  const pool = [...items]
  const out: A[] = []
  while (out.length < count && pool.length > 0) out.push(pool.splice(int(rng, 0, pool.length - 1), 1)[0]!)
  return out
}

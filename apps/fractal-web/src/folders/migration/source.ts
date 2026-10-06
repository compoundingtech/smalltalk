import { Schema } from 'effect'
import { decodeClaim, emptyDoc, mergeDoc } from './legacy.ts'
import type { LegacySource, SourceFence, FrozenSnapshot } from './run.ts'

const cursor = Schema.Number.check(Schema.makeFilter((value) => Number.isSafeInteger(value) && value >= 0))
export interface WriterFence {
  freeze(): Promise<SourceFence>
  assertFrozen(fence: SourceFence): Promise<void>
}
/** Enumerate immutable current persistence only while every source writer is fenced.
 * Transport remains injected; the privileged reader is a private host adapter. */
export const createLegacyClaimsSource = ({ page, fence }: {
  readonly page: (cursor?: string) => Promise<ClaimPage>; readonly fence: WriterFence
}): LegacySource => ({
  freeze: () => fence.freeze(),
  assertFrozen: (receipt) => fence.assertFrozen(receipt),
  readFrozen: async (receipt) => {
    await fence.assertFrozen(receipt)
    const snapshot = await foldLegacyClaims(page)
    await fence.assertFrozen(receipt)
    return snapshot
  },
})

export interface ClaimPage { readonly claims: readonly unknown[]; readonly nextCursor?: string; readonly storeIndex: number }
/** For operator sources that enumerate immutable claims directly, do not stop on an
 * unchanged fold: older pages can contain a winning register hidden by unrelated claims. */
export const foldLegacyClaims = async (page: (cursor?: string) => Promise<ClaimPage>): Promise<FrozenSnapshot> => {
  const doc = emptyDoc()
  let next: string | undefined
  let storeIndex = 0
  const seen = new Set<string>()
  do {
    const current = await page(next)
    Schema.decodeUnknownSync(cursor)(current.storeIndex)
    storeIndex = Math.max(storeIndex, current.storeIndex)
    for (const claim of current.claims) {
      const decoded = decodeClaim(claim)
      if (decoded !== undefined) mergeDoc(doc, decoded)
    }
    next = current.nextCursor
    if (next !== undefined) {
      if (seen.has(next)) throw new TypeError('Legacy claims cursor did not advance')
      seen.add(next)
    }
  } while (next !== undefined)
  return { doc, storeIndex }
}

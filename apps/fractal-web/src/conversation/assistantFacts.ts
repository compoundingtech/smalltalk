import { recordValue } from './blocks.ts'

export interface NativeTurnFacts { readonly model?: string; readonly tokens?: number; readonly costUsd?: number }
const finite = (value: unknown): number | undefined => typeof value === 'number' && Number.isFinite(value) ? value : undefined

/** Last assistant record owns response facts; never sum duplicated block metadata. */
export const nativeTurnFacts = (turn: { readonly items: readonly unknown[] }): NativeTurnFacts | undefined => {
  for (let index = turn.items.length - 1; index >= 0; index -= 1) {
    const metadata = recordValue(recordValue(turn.items[index])?.assistantMetadata)
    if (metadata === undefined) continue
    const usage = recordValue(metadata.usage)
    const model = typeof metadata.model === 'string' ? metadata.model : undefined
    const input = finite(usage?.input), output = finite(usage?.output)
    const tokens = finite(usage?.total) ?? (input !== undefined && output !== undefined ? input + output : undefined)
    const costUsd = finite(usage?.cost_usd) ?? finite(metadata.cost_usd)
    return { ...(model === undefined ? {} : { model }), ...(tokens === undefined ? {} : { tokens }), ...(costUsd === undefined ? {} : { costUsd }) }
  }
  return undefined
}

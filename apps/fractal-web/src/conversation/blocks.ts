import { TimelineBlock } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

export const isRecordValue = (value: unknown): value is Readonly<Record<string, unknown>> =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
export const recordValue = (value: unknown): Readonly<Record<string, unknown>> | undefined =>
  isRecordValue(value) ? value : undefined

const isBlocks = Schema.is(Schema.Array(TimelineBlock))
/** Facts already decoded at the SDK boundary, never guessed from prose. */
export const blockFacts = (body: unknown): {
  readonly blocks?: readonly TimelineBlock[]
  readonly view?: Readonly<Record<string, unknown>>
  readonly assistantMetadata?: Readonly<Record<string, unknown>>
} => {
  if (!isRecordValue(body) || !isBlocks(body.blocks)) return {}
  const blocks = body.blocks
  const view = blocks.find(block => block.view !== undefined)?.view
  const assistant = blocks.find(block => (block.kind === 'text' || block.kind === 'reasoning') && block.metadata !== undefined)?.metadata
  return { blocks, ...(view === undefined ? {} : { view }),
    ...(assistant === undefined ? {} : { assistantMetadata: assistant }) }
}

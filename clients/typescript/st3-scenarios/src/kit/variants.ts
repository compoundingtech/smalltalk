import { rosterVariants } from './variants/roster.ts'
import { detailsVariants } from './variants/details.ts'
import { attentionVariants } from './variants/attention.ts'
import { conversationVariants } from './variants/conversation.ts'
import { terminalVariants } from './variants/terminal.ts'
import { syncVariants } from './variants/sync.ts'
import type { VariantTable } from './world.ts'

export { liveSync, socketSurfaces } from './variants/sync.ts'

/** Generic variants are owned by their slice modules; worlds may override a named variant. */
export const genericVariants: VariantTable = {
  roster: rosterVariants,
  details: detailsVariants,
  attention: attentionVariants,
  conversation: conversationVariants,
  terminal: terminalVariants,
  sync: syncVariants,
}

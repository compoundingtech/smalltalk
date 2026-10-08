import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

export const attentionVariants: VariantTable['attention'] = {
  none: (_ctx, base) => cleared(base.attention, { attention: [], messages: [] }),
}

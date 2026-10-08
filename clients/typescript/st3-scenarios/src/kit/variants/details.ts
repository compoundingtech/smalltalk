import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

export const detailsVariants: VariantTable['details'] = {
  empty: (_ctx, base) => cleared(base.details, { missions: [], work: [] }),
  loading: (_ctx, base) => ({ ...base.details, loading: true }),
}

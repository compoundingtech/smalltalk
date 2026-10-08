import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

export const conversationVariants: VariantTable['conversation'] = {
  empty: (ctx, base) => {
    const threads = base.conversation.state.threads.length === 0
      ? ctx.cast.agents.map((member) => ({ agent: member.id, session_id: member.session, items: [], has_more: false, page_size: 100 }))
      : base.conversation.state.threads.map((thread) => ({ ...thread, items: [], has_more: false }))
    return cleared(base.conversation, { threads })
  },
  loading: (_ctx, base) => ({ ...base.conversation, loading: true }),
}

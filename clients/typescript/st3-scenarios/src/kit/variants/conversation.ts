import type { TimelineEntry } from '@smalltalk/st3-client'
import { child, type FactoryContext } from '../context.ts'
import { ENTRIES_PER_HISTORY_TURN, generateConversationRange, HUGE_COMMITTED_TURNS, HUGE_TURNS } from '../conversationHistory.ts'
import { diff } from '../factories/diff.ts'
import { toolCall } from '../factories/toolCall.ts'
import { entry, revise, thread, turn, type TurnInput } from '../factories/turn.ts'
import type { ConversationEvent, ConversationHistory, ConversationState } from '../slice.ts'
import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

const conversation = (ctx: FactoryContext, mode: 'long' | 'tool-heavy' | 'failed-tools' | 'remote-only-mail'): ConversationState => ({
  threads: ctx.cast.agents.map((member, index) => {
    const c = child(ctx, `conversation/${mode}/${member.key}`)
    const cursor = thread(member)
    const person = ctx.cast.people[0]
    // Mail endpoints need not differ. Prefer a remote peer, then a local peer; a singleton
    // seat sends a durable continuation to itself rather than inventing a second agent.
    const sender = ctx.cast.agents.find((candidate) => candidate.id !== member.id && candidate.host.id !== member.host.id)
      ?? ctx.cast.agents.find((candidate) => candidate.id !== member.id)
      ?? member
    let from: TurnInput['from']
    if (mode === 'remote-only-mail') from = { _tag: 'mail', agent: sender, title: sender.id === member.id ? 'Session-aware migration continuation' : 'Session-aware migration handoff' }
    else {
      if (person === undefined) throw new Error('conversation variant requires a declared cast person')
      from = { _tag: 'person', person }
    }
    const items: TimelineEntry[] = []
    const count = mode === 'long' ? 80 : mode === 'remote-only-mail' ? 3 : 1
    for (let exchange = 0; exchange < count; exchange += 1) {
      const atMs = -((count - exchange) * 30_000) - index * 1_000
      items.push(...turn(c, cursor, { atMs, from, text: `${mode === 'remote-only-mail' && sender.id === member.id ? 'Resume' : 'Review'} migration batch ${exchange + 1}: preserve the session argument at every caller.`, steps: [{ _tag: 'say', text: 'I checked the callers and am validating the session-aware signature.' }], status: mode === 'failed-tools' ? 'waiting' : 'completed', stepMs: 1_000 }))
    }
    if (mode === 'tool-heavy' || mode === 'failed-tools') {
      for (let call = 0; call < 5; call += 1) items.push(...toolCall(c, cursor, { atMs: -20_000 + call * 3_000, name: 'shell', arguments: { command: call === 0 ? 'git diff -- src/session.ts' : 'pnpm typecheck' }, outcome: call === 4 ? 'pending' : mode === 'failed-tools' ? 'error' : 'ok', output: mode === 'failed-tools' ? 'error TS2554: Expected 2 arguments, but got 1.' : 'Typecheck passed.', durationMs: 500 }))
      items.push(entry(c, cursor, { atMs: -3_000, role: 'assistant', type: 'content', body: { media_type: 'text/markdown', text: mode === 'failed-tools' ? 'The caller still omits the session. I will repair it; the final rerun is pending.' : 'The callers now pass their session; the final validation is running.' } }))
      if (mode === 'tool-heavy') items.push(...diff(c, cursor, { atMs: -2_000, file: 'src/session.ts', before: 'loadUser(id)', after: 'loadUser(session, id)' }))
    }
    return { agent: member.id, session_id: member.session, items, page_size: 100, has_more: items.length > 100 }
  }),
})

export const conversationVariants: VariantTable['conversation'] = {
  empty: (ctx, base) => cleared(base.conversation, { threads: ctx.cast.agents.map((member) => ({ agent: member.id, session_id: member.session, items: [], has_more: false, page_size: 100 })) }),
  loading: (_ctx, base) => ({ ...base.conversation, loading: true }),
  streaming: (ctx, base) => {
    const timeline: ConversationEvent[] = []
    const threads = ctx.cast.agents.map((member) => {
      const c = child(ctx, `stream/${member.key}`)
      const cursor = thread(member)
      const first = entry(c, cursor, { atMs: -500, role: 'assistant', type: 'content', final: false, body: { media_type: 'text/markdown', text: 'Tracing the session' } })
      const second = revise(c, first, 1_000, { media_type: 'text/markdown', text: 'Tracing the session argument through each caller.' }, false)
      const final = revise(c, second, 2_000, { media_type: 'text/markdown', text: 'Tracing complete: every caller preserves its session argument.' }, true)
      timeline.push({ _tag: 'entries', at_ms: 1_000, store: 0, agent: member.id, items: [second] }, { _tag: 'entries', at_ms: 2_000, store: 0, agent: member.id, items: [final] })
      return { agent: member.id, session_id: member.session, items: [first], page_size: 100, has_more: false }
    })
    timeline.sort((a, b) => a.at_ms - b.at_ms)
    return { ...cleared(base.conversation, { threads }), timeline }
  },
  long: (ctx, base) => cleared(base.conversation, conversation(ctx, 'long')),
  'tool-heavy': (ctx, base) => cleared(base.conversation, conversation(ctx, 'tool-heavy')),
  'failed-tools': (ctx, base) => cleared(base.conversation, conversation(ctx, 'failed-tools')),
  'remote-only-mail': (ctx, base) => cleared(base.conversation, conversation(ctx, 'remote-only-mail')),
  huge: (ctx, base) => {
    const member = ctx.cast.agents[0]
    if (member === undefined) throw new Error('huge conversation requires a cast agent')
    const totalEntries = HUGE_TURNS * ENTRIES_PER_HISTORY_TURN
    const committedEntries = HUGE_COMMITTED_TURNS * ENTRIES_PER_HISTORY_TURN
    const history: ConversationHistory = {
      kind: 'seeded-turns',
      seed: base.conversation.source._tag === 'synthetic' ? base.conversation.source.seed : ctx.rng.seed[0],
      world: ctx.world,
      total_turns: HUGE_TURNS,
      total_entries: totalEntries,
      committed_from_sequence: totalEntries - committedEntries + 1,
      next_cursor: `scenario-cursor/${committedEntries}`,
    }
    return cleared(base.conversation, { threads: [{
      agent: member.id,
      session_id: member.session,
      items: generateConversationRange(ctx, member, history, history.committed_from_sequence, totalEntries),
      page_size: 50,
      has_more: true,
      history,
    }] })
  },
}

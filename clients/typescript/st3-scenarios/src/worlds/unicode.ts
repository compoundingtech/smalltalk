import { child } from '../kit/context.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { thread, turn } from '../kit/factories/turn.ts'
import { liveSync } from '../kit/variants.ts'
import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

// Deliberately retain decomposed accents and the ZWJ sequence without normalization.
export const UNICODE_LONG_NAME = '界'.repeat(200)
export const UNICODE_PATH = `~/src/atlas/${'display-width/'.repeat(16)}fixtures`
export const UNICODE_TEXT = `واجهة العربية — 日本語の表示 — Cafe\u0301 — 👩🏽‍💻 🚀 — ${UNICODE_LONG_NAME} — ${UNICODE_PATH}`

/** Display text stresses shaping and width; public identifiers remain ASCII. */
export const unicode: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'unicode',
  title: 'Unicode display and content',
  narrative: 'An internationalization review exercises RTL, CJK, decomposed accents, emoji and unusually long labels.',
  cast: {
    ...fleetMidRefactor.cast,
    roles: undefined,
    agents: [
      { key: 'builder', role: 'builder', name: UNICODE_LONG_NAME, workspace: UNICODE_PATH },
      { key: 'reviewer', role: 'reviewer', name: UNICODE_TEXT },
      { key: 'migrator', role: 'migrator', name: '日本語の移行 👩🏽‍💻' },
      { key: 'docs', role: 'docs', name: 'دليل الواجهة Cafe\u0301' },
    ],
    missions: [{ slug: 'display-review', title: UNICODE_TEXT, steps: [UNICODE_TEXT, '日本語の折り返し', 'مراجعة اتجاه النص', 'Cafe\u0301 remains decomposed', UNICODE_LONG_NAME] }],
  },
  slices: (ctx) => {
    const base = fleetMidRefactor.slices(ctx)
    const member = ctx.cast.agents[0]!
    const conversation = {
      ...base.conversation,
      state: { threads: [{ agent: member.id, session_id: member.session, page_size: 50, has_more: false,
        items: turn(child(ctx, 'unicode/conversation'), thread(member), {
          atMs: -60_000, from: { _tag: 'person', person: ctx.cast.people[0]! }, text: UNICODE_TEXT,
          steps: [{ _tag: 'say', text: `Preserve exact code points: ${UNICODE_TEXT}` }],
        }),
      }] },
      timeline: [],
    }
    const run = terminalRun(child(ctx, 'unicode/terminal'), member, {
      startedAtMs: -10_000, command: 'pnpm test display-width', lines: [UNICODE_TEXT, `${UNICODE_LONG_NAME}: preserved`],
    })
    const sync = liveSync(ctx, { conversation })
    return {
      ...base,
      roster: { ...base.roster, timeline: [] },
      attention: { ...base.attention, state: {
        attention: base.attention.state.attention.map((card) => ({ ...card, title: UNICODE_TEXT, detail: UNICODE_TEXT })),
        messages: base.attention.state.messages.map((message) => ({ ...message, title: UNICODE_TEXT, content: UNICODE_TEXT })),
      }, timeline: [] },
      conversation,
      terminal: { ...base.terminal, state: { terminals: [terminalRecord(ctx, member, run, -10_000)] }, timeline: [] },
      sync: { ...sync, state: { ...sync.state, capabilities: { ...sync.state.capabilities, machine_version: `example-display-${UNICODE_TEXT}` } } },
    }
  },
}

import { child, type FactoryContext } from '../context.ts'
import { terminalRecord } from '../factories/terminalRecord.ts'
import { terminalRun } from '../factories/terminalRun.ts'
import type { TerminalRecord } from '../slice.ts'
import * as vocabulary from '../vocabulary/index.ts'
import type { VariantTable } from '../world.ts'
import { cleared } from './shared.ts'

const recordFor = (ctx: FactoryContext): TerminalRecord => {
  const member = ctx.cast.agents[0]
  if (member === undefined) throw new Error('terminal variants require a cast agent')
  const run = terminalRun(child(ctx, 'fallback'), member, { startedAtMs: -10_000, command: vocabulary.shellRuns[1].command, lines: vocabulary.shellRuns[1].lines })
  return terminalRecord(ctx, member, run, -10_000)
}

export const terminalVariants: VariantTable['terminal'] = {
  none: (_ctx, base) => cleared(base.terminal, { terminals: [] }),
  running: (ctx, base) => {
    const owner = ctx.cast.agents[0]
    if (owner === undefined) throw new Error('terminal owner must belong to the cast')
    const run = terminalRun(child(ctx, 'running'), owner, {
      startedAtMs: -1_500,
      command: vocabulary.shellRuns[1].command,
      lines: vocabulary.shellRuns[1].lines,
      running: true,
    })
    const next = terminalRecord(ctx, owner, run, -1_500)
    return {
      ...cleared(base.terminal, { terminals: [next] }),
      timeline: run.screens.filter((screen) => screen.at_ms > 0)
        .map((screen) => ({ _tag: 'screen' as const, at_ms: screen.at_ms, store: 0, terminal: next.terminal, screen: screen.screen })),
    }
  },
  unavailable: (ctx, base) => {
    const record = recordFor(ctx)
    return { ...cleared(base.terminal, { terminals: [record] }), timeline: [{ _tag: 'unavailable', at_ms: 3_000, store: 0, terminal: record.terminal }] }
  },
  exited: (ctx, base) => {
    const record = recordFor(ctx)
    return { ...cleared(base.terminal, { terminals: [record] }), timeline: [{ _tag: 'end', at_ms: 3_000, store: 0, terminal: record.terminal }] }
  },
  restarted: (ctx, base) => {
    const record = recordFor(ctx)
    const incarnation = record.incarnation.replace(/:(\d+)$/, (_, n: string) => `:${Number(n) + 1}`)
    const screen = record.screens.at(-1)?.screen
    if (screen === undefined) throw new Error('restarted terminal variant requires a screen')
    return {
      ...cleared(base.terminal, { terminals: [record] }),
      timeline: [
        { _tag: 'incarnation', at_ms: 3_000, store: 0, terminal: record.terminal, incarnation },
        { _tag: 'screen', at_ms: 3_500, store: 0, terminal: record.terminal, screen: { ...screen, runtime_incarnation: incarnation, revision: `${screen.revision}-r` } },
      ],
    }
  },
}

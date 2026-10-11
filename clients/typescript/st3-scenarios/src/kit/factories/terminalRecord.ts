import type { CastAgent } from '../cast.ts'
import type { FactoryContext } from '../context.ts'
import type { TerminalRecord } from '../slice.ts'
import type { TerminalRunResult } from './terminalRun.ts'

/** Screens up to offset zero are state; later screens become timeline events. */
export const terminalRecord = (
  ctx: FactoryContext,
  member: CastAgent,
  run: TerminalRunResult,
  startedMs: number,
): TerminalRecord => ({
  terminal: member.terminal,
  owner: member.id,
  runtime: {
    id: member.terminalRuntime,
    kind: 'runtime',
    revision: 'rt-shell-1',
    updated_at: ctx.t.at(startedMs),
    runtime_kind: 'terminal',
    owner_id: member.id,
    owner_host_id: member.host.id,
    state: 'running',
    runtime_id: member.terminalRuntime.replace('runtime/', ''),
    incarnation_id: member.terminalIncarnation,
    desired_revision: 'desired-1',
    terminal_id: member.terminal,
  },
  incarnation: member.terminalIncarnation,
  cast: run.cast,
  screens: run.screens.filter((screen) => screen.at_ms <= 0),
})

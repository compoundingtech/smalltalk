import type { TerminalScreen } from '@smalltalk/st3-client'

import type { Asciicast } from '../asciicast.ts'
import type { CastAgent } from '../cast.ts'
import type { FactoryContext } from '../context.ts'
import { int } from '../rng.ts'
import { screenAt } from '../screen.ts'

export interface TerminalRunInput {
  readonly startedAtMs: number
  readonly command: string
  readonly lines: readonly string[]
  readonly exit?: number
  /** Keep the command running: no prompt after the output. */
  readonly running?: boolean
  readonly width?: number
  readonly height?: number
}

export interface TerminalRunResult {
  readonly cast: Asciicast
  /** One screen per output event, at its offset from `now`. */
  readonly screens: { readonly at_ms: number; readonly screen: TerminalScreen }[]
}

/** An asciicast v2 recording of one command and the screens a client sees for it. */
export const terminalRun = (ctx: FactoryContext, member: CastAgent, input: TerminalRunInput): TerminalRunResult => {
  const prompt = `\u001b[32m${member.project}\u001b[0m:${member.role} $ `
  const events: [number, 'o', string][] = [[0, 'o', prompt + input.command + '\r\n']]
  let seconds = 0.2
  for (const line of input.lines) {
    seconds = Math.round((seconds + int(ctx.rng, 80, 900) / 1000) * 1000) / 1000
    events.push([seconds, 'o', line + '\r\n'])
  }
  if (input.running !== true) events.push([Math.round((seconds + 0.05) * 1000) / 1000, 'o', prompt])
  const cast: Asciicast = {
    header: { version: 2, width: input.width ?? 100, height: input.height ?? 12, title: input.command },
    started_at_ms: input.startedAtMs,
    events,
  }
  const screens = events.map(([time]) => ({
    at_ms: input.startedAtMs + Math.round(time * 1000),
    screen: screenAt(cast, time, { terminalId: member.terminal, incarnation: member.terminalIncarnation }),
  }))
  return { cast, screens }
}

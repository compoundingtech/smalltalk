import * as React from 'react'
import type { TerminalFrame, TerminalSession } from './terminal-model'

const supported = { state: 'supported' } as const
export const terminalSessions: readonly TerminalSession[] = [
  { ref: 'terminal/worker-1', title: 'Checks', agentRef: 'agent/worker-1', cwd: 'sample', command: 'pnpm vitest run sample', status: 'running', output: [{ text: '$ pnpm vitest run sample', tone: 'prompt' }, { text: 'sample/check.ts: 1 passed (4 rows) · 184 ms', tone: 'info' }, { text: '$ ', tone: 'prompt', cursor: true }] },
  { ref: 'terminal/worker-2', title: 'Generate', agentRef: 'agent/worker-1', cwd: 'sample', command: 'regenerate sample/output.json', status: 'exited', output: [{ text: '$ regenerate sample/output.json', tone: 'prompt' }, { text: 'wrote sample/output.json (4 rows)' }, { text: 'process exited (0)', tone: 'info' }] },
  { ref: 'terminal/worker-3', title: 'Shell', agentRef: 'agent/worker-1', cwd: 'sample', command: 'shell', status: 'running', output: [{ text: '$ pwd', tone: 'prompt' }, { text: '/workspace/sample' }, { text: '$ ', tone: 'prompt', cursor: true }] },
]
export const terminalFixtureFrame: TerminalFrame = { state: 'supported', sessions: terminalSessions, add: supported, kill: supported }
export const oneTerminalFixtureFrame: TerminalFrame = { ...terminalFixtureFrame, sessions: terminalSessions.slice(0, 1) }
export const emptyTerminalFixtureFrame: TerminalFrame = { ...terminalFixtureFrame, sessions: [] }
export const addUnsupportedTerminalFrame: TerminalFrame = { ...oneTerminalFixtureFrame, add: { state: 'unsupported', reason: 'This workspace does not allow creating terminal sessions.' } }
export const unsupportedTerminalFrame: TerminalFrame = { state: 'unsupported', reason: 'This harness does not expose terminal sessions.' }


export function useTerminalFixture(initial: TerminalFrame) {
  const [frame, setFrame] = React.useState(initial)
  const sequence = React.useRef(0)
  const add = React.useCallback((agentRef = 'agent/worker-1'): string | undefined => {
    if (frame.state !== 'supported' || frame.add.state !== 'supported') return undefined
    const number = ++sequence.current
    const ref = `terminal/fixture-${number}`
    const session: TerminalSession = { ref, title: `Shell ${number}`, agentRef, cwd: 'sample', command: 'shell', status: 'running', output: [{ text: '$ pwd', tone: 'prompt' }, { text: '/workspace/sample' }, { text: '$ ', tone: 'prompt', cursor: true }] }
    setFrame(current => current.state === 'supported' ? { ...current, sessions: [...current.sessions, session] } : current)
    return ref
  }, [frame])
  const kill = React.useCallback((ref: string) => {
    setFrame(current => current.state === 'supported' && current.kill.state === 'supported' ? { ...current, sessions: current.sessions.map((session): TerminalSession => session.ref === ref ? { ...session, status: 'exited', output: [...session.output, { text: 'session killed', tone: 'error' }] } : session) } : current)
  }, [])
  return { frame, add, kill }
}

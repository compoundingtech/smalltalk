/** Observed support is distinct from unavailable support and unobserved support. */
export type TerminalCapability =
  | { readonly state: 'supported' }
  | { readonly state: 'unsupported'; readonly reason: string }
  | { readonly state: 'unknown' }

export interface TerminalOutputLine {
  readonly text: string
  readonly tone?: 'prompt' | 'info' | 'error'
  readonly cursor?: boolean
}

/** Session-shaped fixture source, not a live PTY connection. */
export interface TerminalSession {
  readonly ref: string
  readonly title: string
  readonly agentRef: string
  readonly cwd: string
  readonly command: string
  readonly status: 'running' | 'exited'
  readonly output: readonly TerminalOutputLine[]
}

export type TerminalFrame =
  | { readonly state: 'unknown' }
  | { readonly state: 'unsupported'; readonly reason: string }
  | {
      readonly state: 'supported'
      readonly sessions: readonly TerminalSession[]
      readonly add: TerminalCapability
      readonly kill: TerminalCapability
    }

export const unknownTerminalFrame: TerminalFrame = { state: 'unknown' }

export const terminalCapabilityReason = (capability: TerminalCapability, action: string): string | undefined =>
  capability.state === 'unsupported' ? capability.reason : capability.state === 'unknown' ? `${action} support has not been reported.` : undefined

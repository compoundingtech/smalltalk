import { ClientError, plainError, type St3Client } from '@smalltalk/st3-client'

/** A change to a seat, as stui's agent actions name them. */
export type AgentControl = 'restart' | 'suspend' | 'retire' | 'start' | 'resume'

export type AgentControlResult =
  | { readonly _tag: 'Sent' }
  | { readonly _tag: 'Refused'; readonly detail: string }

export interface AgentControlPort {
  /** Restart, suspend, retire, start or resume a seat, fenced on its current declaration. */
  readonly control: (agent: string, control: AgentControl) => Promise<AgentControlResult>
  /** Stop the agent's current turn: Esc in its own terminal, as stui sends it. */
  readonly interrupt: (agent: string) => Promise<AgentControlResult>
}

const REASON = "from the web app's agent actions"

const refused = (detail: string): AgentControlResult => ({ _tag: 'Refused', detail })

/** The seat's live runtime, as st holds it now: its incarnation and terminal, or why there is none. */
const liveRuntime = async (client: St3Client, agent: string): Promise<{ readonly incarnation?: string; readonly terminal?: string } | string> => {
  const row = (await client.agentsGet(agent)).value as { kind?: unknown; runtime_ids?: readonly string[] }
  const runtimeId = row.kind === 'agent' ? row.runtime_ids?.[0] : undefined
  if (runtimeId === undefined) return 'That agent is not running.'
  const runtime = (await client.runtimesGet(runtimeId)).value as { kind?: unknown; incarnation_id?: string | null; terminal_id?: string | null }
  if (runtime.kind !== 'runtime') return 'That agent is not running.'
  return { ...(runtime.incarnation_id ? { incarnation: runtime.incarnation_id } : {}), ...(runtime.terminal_id ? { terminal: runtime.terminal_id } : {}) }
}
const explained = (error: unknown): AgentControlResult =>
  refused(error instanceof ClientError ? `${plainError(error)}.` : 'st did not confirm it; check the agent before trying again. It was not sent again.')

/**
 * One action per step, each fenced to the seat's declaration as st selects it at that moment. A
 * restart is a stop and then a start, as stui does it; nothing is retried.
 */
export const gatewayAgentControls = (client: St3Client): AgentControlPort => ({
  control: async (agent, control) => {
    const steps: readonly Exclude<AgentControl, 'restart'>[] = control === 'restart' ? ['retire', 'start'] : [control]
    // A restart whose stop landed and whose start did not leaves the seat stopped: say so.
    const after = (index: number, result: AgentControlResult): AgentControlResult =>
      control === 'restart' && index > 0 && result._tag === 'Refused'
        ? refused(`Stopped, but starting it again failed: ${result.detail} Start it from this menu.`)
        : result
    let index = 0
    try {
      for (const [at, step] of steps.entries()) {
        index = at
        const definition = await client.subjectDefinition(agent)
        const fence = { snapshot_id: definition.snapshot.id, subject_revisions: {}, runtime_desired_revision: definition.value.desired_token }
        const id = `action/${crypto.randomUUID()}`
        const action = { id, idempotency_key: id, fence }
        let response
        if (step === 'suspend') {
          // A suspension is fenced on the running incarnation too.
          const runtime = await liveRuntime(client, agent)
          if (typeof runtime === 'string') return after(at, refused(runtime))
          if (runtime.incarnation === undefined) return after(at, refused('st does not say which process is running, so it cannot be suspended now.'))
          response = await client.agentSuspend({ ...action, fence: { ...fence, runtime_incarnation: runtime.incarnation }, parameters: { agent, reason: REASON } })
        } else {
          response =
            step === 'retire' ? await client.agentStop({ ...action, parameters: { agent, reason: REASON } })
              : step === 'start' ? await client.agentStart({ ...action, parameters: { agent } })
                : await client.agentResume({ ...action, parameters: { agent } })
        }
        if ((response.value as { status?: unknown }).status === 'rejected') return after(at, refused('st did not take this.'))
      }
      return { _tag: 'Sent' }
    } catch (error) {
      return after(index, explained(error))
    }
  },
  interrupt: async (agent) => {
    try {
      const runtime = await liveRuntime(client, agent)
      if (typeof runtime === 'string') return refused(runtime)
      if (runtime.terminal === undefined) return refused('That agent has no terminal to stop its turn through.')
      const screen = await client.terminalScreen(runtime.terminal)
      const id = `action/${crypto.randomUUID()}`
      const response = await client.terminalInput({
        id,
        idempotency_key: id,
        fence: {
          snapshot_id: screen.snapshot.id,
          subject_revisions: {},
          runtime_incarnation: screen.value.runtime_incarnation,
          terminal_sequence: screen.value.next_sequence,
        },
        parameters: { terminal_id: runtime.terminal, mode: 'key', value: 'escape' },
      })
      return (response.value as { status?: unknown }).status === 'rejected' ? refused('st did not take this.') : { _tag: 'Sent' }
    } catch (error) {
      return explained(error)
    }
  },
})

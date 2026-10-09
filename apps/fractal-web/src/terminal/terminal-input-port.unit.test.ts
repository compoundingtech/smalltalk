import { St3Client } from '@smalltalk/st3-client'
import type { TerminalScreen } from '@smalltalk/st3-client/schema'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import recording from '../data/subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import { makeScreen } from './fixtures.ts'
import { gatewayTerminalInput } from './terminal-input-port.ts'

const ref = 'terminal/example'
const ack = {
  kind: 'action-result', action_id: 'action/returned', operation_id: 'operation/returned',
  snapshot_id: 'snapshot/returned', affected_ids: [ref], status: 'accepted',
}
const envelope = (code: string, status: number) =>
  Response.json({
    api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code,
    message: 'refused', retryable: false, request_id: 'request/example', details: {},
  }, { status })

let registry: AtomRegistry.AtomRegistry
beforeEach(() => {
  registry = AtomRegistry.make()
})
afterEach(() => registry.dispose())

const harness = ({
  reply = () => Response.json({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/native' }, value: ack }),
  snapshot,
  credential,
}: {
  readonly reply?: () => Response | Promise<Response>
  readonly snapshot?: () => Promise<string | undefined>
  readonly credential?: () => Promise<string>
} = {}) => {
  let granted = true
  const actions: unknown[] = []
  let screen: TerminalScreen | undefined = { ...makeScreen({ scene: 'F3', columns: 40, rows: 4, frame: 0 }), terminal_id: ref, runtime_incarnation: 'incarnation/one', next_sequence: 11 }
  let snapshots = 0
  const transport: typeof fetch = async (input, init) => {
    const url = new URL(String(input))
    if (url.pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
    if (typeof init?.body === 'string') actions.push(JSON.parse(init.body))
    return reply()
  }
  const factory = gatewayTerminalInput({
    connect: (fetchImpl) => new St3Client({ baseUrl: 'https://gateway.invalid', fetchImpl, ...(credential === undefined ? {} : { credential }) }),
    transport,
    snapshot: snapshot ?? (async () => `snapshot/fresh/${(snapshots += 1)}`),
    liveScreen: (terminalRef) => (terminalRef === ref ? screen : undefined),
    granted: () => granted,
  })
  return {
    actions,
    revoke: () => {
      granted = false
    },
    setScreen: (next: TerminalScreen | undefined) => {
      screen = next
    },
    screen: () => screen!,
    open: async (incarnation = 'incarnation/one') => {
      const port = await factory({ terminalRef: ref, incarnation, registry })
      port.open()
      return port
    },
  }
}

describe('gateway terminal input', () => {
  it('posts terminal.input with fences read fresh for every chunk', async () => {
    const gateway = harness()
    const port = await gateway.open()
    await port.send(new Uint8Array([3]))
    gateway.setScreen({ ...gateway.screen(), next_sequence: 12 })
    await port.send(new TextEncoder().encode('ls'))
    expect(gateway.actions).toEqual([
      expect.objectContaining({
        type: 'terminal.input',
        parameters: { terminal_id: ref, mode: 'key', value: 'ctrl+c' },
        fence: { snapshot_id: 'snapshot/fresh/1', subject_revisions: {}, runtime_incarnation: 'incarnation/one', terminal_sequence: 11 },
      }),
      expect.objectContaining({
        parameters: { terminal_id: ref, mode: 'raw', value: btoa('ls') },
        fence: { snapshot_id: 'snapshot/fresh/2', subject_revisions: {}, runtime_incarnation: 'incarnation/one', terminal_sequence: 12 },
      }),
    ])
    const [first, second] = gateway.actions as Array<{ id: string; idempotency_key: string }>
    expect(first!.id).not.toBe(second!.id)
    expect(first!.idempotency_key).toBe(first!.id)
  })

  it('sends nothing to a restarted, switched or stale terminal and drops its queue', async () => {
    const gateway = harness()
    const port = await gateway.open()
    gateway.setScreen({ ...gateway.screen(), runtime_incarnation: 'incarnation/two' })
    await expect(port.send(new TextEncoder().encode('x'))).rejects.toThrow('restarted or stopped updating')
    const stale = harness()
    const other = await stale.open()
    stale.setScreen(undefined)
    await expect(other.send(new TextEncoder().encode('y'))).rejects.toThrow('restarted or stopped updating')
    expect([...gateway.actions, ...stale.actions]).toEqual([])
    expect(registry.get(port.state)).toMatchObject({ _tag: 'Closed', uncertain: false })
  })

  it.each([
    ['stale-fence', 409, 'The terminal changed before this input arrived. Queued keys were dropped.', false],
    ['forbidden', 403, 'This device is not allowed to type into this terminal.', false],
    ['validation', 400, 'The gateway refused this input. Queued keys were dropped.', false],
    ['unavailable', 503, 'Input delivery could not be confirmed. Queued keys were dropped.', true],
  ] as const)('stops on %s with fixed copy and no resend', async (code, status, reason, uncertain) => {
    const gateway = harness({ reply: () => envelope(code, status) })
    const port = await gateway.open()
    const sent = port.send(new TextEncoder().encode('a'))
    const queued = port.send(new TextEncoder().encode('b'))
    await expect(sent).rejects.toThrow(reason)
    await expect(queued).rejects.toThrow(reason)
    expect(gateway.actions).toHaveLength(1)
    expect(registry.get(port.state)).toEqual({ _tag: 'Closed', reason, uncertain })
  })

  it('treats a dropped connection as uncertain delivery', async () => {
    const gateway = harness({ reply: () => Promise.reject(new TypeError('Failed to fetch')) })
    const port = await gateway.open()
    await expect(port.send(new TextEncoder().encode('a'))).rejects.toThrow('could not be confirmed')
    expect(registry.get(port.state)).toMatchObject({ _tag: 'Closed', uncertain: true })
  })

  it.each([
    ['the session closes', 'close', 'Input is off. Start a new input session to type again.'],
    ['the terminal input grant is withdrawn', 'revoke', 'This device is not allowed to type into terminals.'],
  ] as const)('posts nothing when %s while the fresh snapshot is still being read', async (_case, cut, reason) => {
    const read = Promise.withResolvers<string | undefined>()
    const gateway = harness({ snapshot: () => read.promise })
    const port = await gateway.open()
    const sent = port.send(new TextEncoder().encode('a')).then(() => 'sent', (error: Error) => error.message)
    // Let the submission reach its snapshot read before the session is cut.
    for (let turn = 0; turn < 10; turn += 1) await Promise.resolve()
    if (cut === 'close') port.close()
    else gateway.revoke()
    read.resolve('snapshot/late')
    expect(await sent).toBe(reason)
    // The abandoned submission runs to its end before anything is asserted about the wire.
    for (let turn = 0; turn < 50; turn += 1) await Promise.resolve()
    expect(gateway.actions).toEqual([])
    expect(registry.get(port.state)).toEqual({ _tag: 'Closed', reason, uncertain: false })
  })

  it.each([
    ['the session closes', 'close', 'Input is off. Start a new input session to type again.'],
    ['the terminal input grant is withdrawn', 'revoke', 'This device is not allowed to type into terminals.'],
    ['the terminal restarts', 'restart', 'The terminal restarted or stopped updating. Queued keys were dropped.'],
  ] as const)('posts nothing when %s after the action call starts but before its request reaches fetch', async (_case, cut, reason) => {
    // The client awaits discovery and then the credential before it fetches; hold the post's credential.
    const held = Promise.withResolvers<string>()
    const reached = Promise.withResolvers<void>()
    let credentials = 0
    const gateway = harness({
      credential: () => {
        credentials += 1
        if (credentials === 1) return Promise.resolve('credential/discovery')
        reached.resolve()
        return held.promise
      },
    })
    const port = await gateway.open()
    const sent = port.send(new TextEncoder().encode('a')).then(() => 'sent', (error: Error) => error.message)
    await reached.promise
    if (cut === 'close') port.close()
    else if (cut === 'revoke') gateway.revoke()
    else gateway.setScreen({ ...gateway.screen(), runtime_incarnation: 'incarnation/two' })
    held.resolve('credential/post')
    expect(await sent).toBe(reason)
    for (let turn = 0; turn < 50; turn += 1) await Promise.resolve()
    expect(gateway.actions).toEqual([])
    expect(registry.get(port.state)).toEqual({ _tag: 'Closed', reason, uncertain: false })
  })

  it('reports a session closed during an unanswered post as uncertain and sends nothing again', async () => {
    const answer = Promise.withResolvers<Response>()
    const posted = Promise.withResolvers<void>()
    const gateway = harness({
      reply: () => {
        posted.resolve()
        return answer.promise
      },
    })
    const port = await gateway.open()
    const sent = port.send(new TextEncoder().encode('a')).then(() => 'sent', (error: Error) => error.message)
    const queued = port.send(new Uint8Array([3])).then(() => 'sent', (error: Error) => error.message)
    await posted.promise
    port.close()
    const reason = 'The last input may have reached the terminal; it was not sent again.'
    expect(registry.get(port.state)).toEqual({ _tag: 'Closed', reason, uncertain: true })
    answer.resolve(Response.json({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/native' }, value: ack }))
    expect(await sent).toBe(reason)
    expect(await queued).toBe(reason)
    expect(gateway.actions).toHaveLength(1)
    expect(registry.get(port.state)).toEqual({ _tag: 'Closed', reason, uncertain: true })
  })
})

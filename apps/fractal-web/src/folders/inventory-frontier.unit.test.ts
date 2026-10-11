import type { Arrangement, ArrangementPage, CollectionStreamOptions, EnvelopeOf } from '@smalltalk/st3-client'
import { ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { Effect } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { expect, it, vi } from 'vitest'
import { sidebarFolders, type SidebarGateway } from './client.ts'
import { project } from './core.mts'
import { optimisticArrangement, type SidebarOperation } from './edit.ts'
import { testArrangement, testCapabilities, testSnapshot } from './testGateway.ts'

it('rejects a delayed subscription inventory captured before an accepted rename', async () => {
  const folder = '00000000-0000-7000-8000-000000000002'
  let current = optimisticArrangement(testArrangement(), [{ op: 'folder.create', id: folder, name: 'Before', parent: null, key: 'a0' }])
  let index = 1
  let stream: CollectionStreamOptions | undefined
  let delayNext = false
  let captured: EnvelopeOf<ArrangementPage> | undefined
  const delayed = Promise.withResolvers<EnvelopeOf<ArrangementPage>>()
  const envelope = <T>(value: T): EnvelopeOf<T> => ({ api_version: 'st3.client.v0', request_id: 'request/example', snapshot: { ...testSnapshot, id: `snapshot/example/${index}/proof`, store_index: index }, value })
  const page = (arrangement: Arrangement): EnvelopeOf<ArrangementPage> => envelope({ kind: 'page', collection: 'arrangements', filters: { person: 'person/example' }, items: [structuredClone(arrangement)], page: { limit: 100, has_more: false, next_cursor: null, cursor_expires_at: null } })
  const gateway: SidebarGateway = {
    discover: async () => envelope({ ...testCapabilities, capabilities: [...testCapabilities.capabilities, { id: 'control.arrangements', version: 0, state: 'granted' }] }),
    arrangementsList: async () => {
      const result = page(current)
      if (delayNext) { delayNext = false; captured = result; return delayed.promise }
      return result
    },
    collectionStream: async (options) => {
      stream = options
      const noop = () => undefined
      return { subscribeGlasses: noop, subscribeArrangements: noop, subscribe: noop, subscribeTerminal: noop, subscribeConversation: noop, unsubscribe: noop, close: noop }
    },
    actions: {
      snapshot: Effect.succeed(testSnapshot.id),
      submitAction: (request) => Effect.sync(() => {
        const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
        current = { ...optimisticArrangement(current, operations), revision: 'claim/accepted' }
        index = 2
        return decodeUnknownSync(ActionResult)({ kind: 'action-result', action_id: request.id, operation_id: 'operation/example', snapshot_id: 'snapshot/example/2/proof', status: 'completed', affected_ids: [current.id], arrangement_revision: 'claim/accepted' })
      }),
    },
  }
  const atom = sidebarFolders({ gateway: () => gateway })
  const registry = AtomRegistry.make()
  const unsubscribe = registry.subscribe(atom, () => {})
  const names = () => project(registry.get(atom).doc, []).folders.map((item) => item.name)
  try {
    expect(registry.get(atom).phase).toBe('connecting')
    await vi.waitFor(() => expect(stream).toBeDefined())
    stream?.onFrame({ kind: 'snapshot', items: [], id: 'arrangements-inventory', collection: 'arrangements', has_more: false, order: [], snapshot: testSnapshot })
    await vi.waitFor(() => expect(names()).toEqual(['Before']))
    delayNext = true
    stream?.onFrame({ kind: 'changes', upserts: [], removes: [], id: 'arrangements-inventory', collection: 'arrangements', has_more: false, order: [], snapshot: testSnapshot })
    await vi.waitFor(() => expect(captured).toBeDefined())
    expect(await registry.get(atom).edit([{ op: 'folder.rename', id: folder, name: 'After' }])).toEqual({ _tag: 'Success' })
    expect(names()).toEqual(['After'])
    delayed.resolve(captured!)
    await delayed.promise
    // Drain the released read's promise continuations, without requesting a repair read.
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(names()).toEqual(['After'])
    expect(registry.get(atom).phase).toBe('synced')
  } finally { unsubscribe(); registry.dispose() }
})

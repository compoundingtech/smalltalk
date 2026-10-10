import { St3Client, type ActionOf, type Arrangement, type ErrorEnvelope } from '@smalltalk/st3-client'
import { ArrangementEditParameters, ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { ActionRefused, ActionTransportFailure, arrangementActions, type ActionFailure } from '@st3/sdk/effect'
import { Effect, Schema } from 'effect'
import { describe, expect, expectTypeOf, it, vi } from 'vitest'
import { arrangementSidebarDoc } from './client.ts'
import { project } from './core.mts'
import { arrangementRefusal, arrangementUuid, createArrangementEditor, optimisticArrangement, refusalText, sidebarOperations, type ArrangementEditorState, type SidebarOperation } from './edit.ts'
import { testArrangement, testCapabilities, testSelection, testSnapshot } from './testGateway.ts'

const folder = '00000000-0000-7000-8000-000000000002'
const other = '00000000-0000-7000-8000-000000000003'
const create: SidebarOperation = { op: 'folder.create', id: folder, name: 'Inbox', parent: null, key: 'a0' }
const rename: SidebarOperation = { op: 'folder.rename', id: folder, name: 'Renamed' }
const refused = (code: string, message = 'The edit was refused.'): ActionRefused => new ActionRefused({
  status: 409, message,
  response: { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/example', code, message, retryable: false, details: {} },
})
const harness = (initial: readonly Arrangement[] = [optimisticArrangement(testArrangement(), [create])], retired: readonly string[] = []) => {
  let items = [...initial]
  let behavior: ActionFailure | 'rejected' | undefined
  let gate: Promise<void> | undefined
  let reads = 0
  let snapshots = 0
  let snapshotFailure: ActionFailure | undefined
  let hideAccepted = false
  const requests: ActionOf<'arrangement.edit'>[] = []
  const states: ArrangementEditorState[] = []
  const editor = createArrangementEditor({
    owner: testSelection.owner,
    read: async () => { reads++; return { owner: testSelection.owner, items: hideAccepted && requests.length > 0 ? [] : items } },
    actions: {
      snapshot: Effect.suspend(() => { snapshots++; return snapshotFailure === undefined ? Effect.succeed(`snapshot/fresh/${snapshots}`) : Effect.fail(snapshotFailure) }),
      submitAction: (request) => Effect.gen(function* () {
        requests.push(structuredClone(request))
        if (gate !== undefined) yield* Effect.tryPromise({ try: () => gate!, catch: (cause) => new ActionTransportFailure({ cause, message: 'Gate failed' }) })
        if (behavior !== undefined && behavior !== 'rejected') return yield* Effect.fail(behavior)
        // Retired subjects stay stored but are absent from the live list; admission never resurrects them.
        if (retired.includes(request.parameters.subject)) return yield* Effect.fail(refused('arrangement-retired'))
        if (items.some((item) => item.id === request.parameters.subject) && request.parameters.operations.some((operation) => operation.op === 'create'))
          return yield* Effect.fail(refused('arrangement-exists'))
        if (behavior !== 'rejected') {
          const base = items.find((item) => item.id === request.parameters.subject) ?? { ...testArrangement(), id: request.parameters.subject }
          const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
          items = [...items.filter((item) => item.id !== base.id), { ...optimisticArrangement(base, operations), revision: `claim/accepted-${requests.length}` }]
        }
        return decodeUnknownSync(ActionResult)({ kind: 'action-result', action_id: request.id, operation_id: 'operation/example', snapshot_id: testSnapshot.id, status: behavior === 'rejected' ? 'rejected' : 'completed', affected_ids: [request.parameters.subject], arrangement_revision: `claim/accepted-${requests.length}` })
      }),
    },
    onState: (state) => states.push(state),
  })
  editor.accept({ owner: testSelection.owner, items })
  return {
    editor, requests, states, last: () => states.at(-1)!, items: () => items,
    counters: () => ({ reads, snapshots }),
    behavior: (next: ActionFailure | 'rejected' | undefined) => { behavior = next },
    gate: (next: Promise<void> | undefined) => { gate = next },
    snapshotFailure: (next: ActionFailure | undefined) => { snapshotFailure = next },
    hideAccepted: () => { hideAccepted = true },
    replace: (next: readonly Arrangement[]) => { items = [...next]; editor.accept({ owner: testSelection.owner, items }) },
  }
}

describe('Sidebar generated operations', () => {
  it('offers only folder operations and subject placement, never Sidebar rename or retirement', () => {
    expectTypeOf<SidebarOperation['op']>().toEqualTypeOf<'folder.create' | 'folder.rename' | 'folder.move' | 'folder.delete' | 'subject.place'>()
    expectTypeOf<keyof typeof sidebarOperations>().toEqualTypeOf<'createFolder' | 'renameFolder' | 'moveFolder' | 'deleteFolder' | 'placeSubject'>()
    expect(Object.keys(sidebarOperations)).toEqual(['createFolder', 'renameFolder', 'moveFolder', 'deleteFolder', 'placeSubject'])
  })

  it('maps folder create/rename/move/delete and placement/reorder 1:1 to the generated union', () => {
    const operations = [
      sidebarOperations.createFolder({ id: folder, name: 'Inbox', parent: null, key: 'a0' }),
      sidebarOperations.renameFolder({ id: folder, name: 'Renamed' }),
      sidebarOperations.moveFolder({ id: folder, parent: other, key: 'a1' }),
      sidebarOperations.deleteFolder({ id: folder }),
      sidebarOperations.placeSubject({ subject: 'agent/example', folder, key: 'a2' }),
      sidebarOperations.placeSubject({ subject: 'agent/example', folder: null, key: 'a3' }),
    ]
    expect(operations.map((operation) => operation.op)).toEqual(['folder.create', 'folder.rename', 'folder.move', 'folder.delete', 'subject.place', 'subject.place'])
    expect(Schema.is(Schema.toEncoded(ArrangementEditParameters))({ ...testSelection, operations })).toBe(true)
    expect(operations[2]).toEqual({ op: 'folder.move', id: folder, parent: other, key: 'a1' })
    expect(operations[5]).toEqual({ op: 'subject.place', subject: 'agent/example', folder: null, key: 'a3' })
  })

  it('generates valid lowercase UUIDv7 arrangement and folder identities', () => {
    const uuid = arrangementUuid()
    expect(uuid).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
    expect(Schema.is(Schema.toEncoded(ArrangementEditParameters))({ owner: 'person/example', subject: `arrangement/person/example/${uuid}`, operations: [{ ...create, id: uuid }] })).toBe(true)
  })

  it('optimistically moves, reorders, renames and deletes without mutating native registers', () => {
    const original = optimisticArrangement(testArrangement(), [create, { op: 'folder.create', id: other, name: 'Child', parent: folder, key: 'a0' }, { op: 'subject.place', subject: 'agent/example', folder: other, key: 'a0' }])
    const before = structuredClone(original)
    const renamed = optimisticArrangement(original, [rename, { op: 'folder.move', id: other, parent: null, key: 'a1' }, { op: 'subject.place', subject: 'agent/second', folder, key: 'a1' }])
    expect(project(arrangementSidebarDoc(renamed), ['agent/example', 'agent/second']).folders.map((item) => item.name)).toEqual(['Renamed', 'Child'])
    const deleted = optimisticArrangement(original, [{ op: 'folder.delete', id: other }])
    expect(project(arrangementSidebarDoc(deleted), ['agent/example']).folders[0]?.members).toEqual(['agent/example'])
    expect(original).toEqual(before)
  })
})

describe('memory-only winning Sidebar edits', () => {
  it('paints immediately, submits only the lowest UUIDv7 Sidebar and replaces optimism with the read', async () => {
    const later = { ...testArrangement(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
    const h = harness([later, optimisticArrangement(testArrangement(), [create])])
    const pending = h.editor.edit([rename])
    expect(h.last().phase).toBe('pending')
    expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Renamed')
    expect(await pending).toEqual({ _tag: 'Success' })
    expect(h.requests[0]?.parameters.subject).toBe(testSelection.subject)
    expect(h.requests[0]?.parameters.operations).toEqual([rename])
    expect(h.requests[0]?.fence.subject_revisions).toEqual({ [testSelection.subject]: 'claim/import' })
    expect(h.last().phase).toBe('synced')
    h.editor.close()
  })

  it('rolls back to the newest authoritative state and retains the affected row refusal', async () => {
    const h = harness()
    const gate = Promise.withResolvers<void>()
    h.gate(gate.promise)
    h.behavior(refused('arrangement-folder-deleted', 'This folder was deleted.'))
    const pending = h.editor.edit([rename])
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    h.replace([optimisticArrangement(testArrangement(), [{ ...create, name: 'Remote name' }])])
    expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Renamed')
    gate.resolve()
    expect(await pending).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'arrangement-folder-deleted' }, targets: [folder] })
    expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Remote name')
    expect(h.last().refusal?.detail).toBe('This folder was deleted.')
    h.editor.close()
  })

  it('creates exactly one Sidebar for concurrent first clicks, then edits that same subject', async () => {
    const h = harness([])
    const first = h.editor.edit([create])
    const second = h.editor.edit([{ op: 'folder.create', id: other, name: 'Second', parent: null, key: 'a1' }])
    expect(Object.keys(h.last().arrangement?.body.folders ?? {})).toEqual([folder, other])
    expect(await Promise.all([first, second])).toEqual([{ _tag: 'Success' }, { _tag: 'Success' }])
    expect(h.requests.flatMap((request) => request.parameters.operations).filter((operation) => operation.op === 'create')).toHaveLength(1)
    expect(new Set(h.requests.map((request) => request.parameters.subject)).size).toBe(1)
    expect(h.items()).toHaveLength(1)
    h.editor.close()
  })

  it('converges two editors on one reserved Sidebar and applies the refused edit only on explicit retry', async () => {
    let items: Arrangement[] = []
    const requests: ActionOf<'arrangement.edit'>[] = []
    const states: ArrangementEditorState[][] = [[], []]
    const admitted = Promise.withResolvers<void>()
    const editors = states.map((state) => createArrangementEditor({
      owner: testSelection.owner,
      read: async () => ({ owner: testSelection.owner, items: [...items] }),
      actions: {
        snapshot: Effect.succeed(testSnapshot.id),
        submitAction: (request) => Effect.gen(function* () {
          requests.push(structuredClone(request))
          // Both clients read an empty inventory before either create is admitted.
          if (requests.length === 2) admitted.resolve()
          yield* Effect.promise(() => admitted.promise)
          const existing = items.find((item) => item.id === request.parameters.subject)
          if (existing !== undefined && request.parameters.operations.some((operation) => operation.op === 'create'))
            return yield* Effect.fail(refused('arrangement-exists'))
          const base = existing ?? { ...testArrangement(), id: request.parameters.subject }
          const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
          items = [...items.filter((item) => item.id !== base.id), { ...optimisticArrangement(base, operations), revision: `claim/accepted-${requests.length}` }]
          return decodeUnknownSync(ActionResult)({
            kind: 'action-result', action_id: request.id, operation_id: 'operation/example',
            snapshot_id: testSnapshot.id, status: 'completed', affected_ids: [base.id],
          })
        }),
      },
      onState: (next) => state.push(next),
    }))
    try {
      for (const editor of editors) editor.accept({ owner: testSelection.owner, items: [] })
      const outcomes = await Promise.all(editors.map((editor, index) => editor.edit([{ ...create, id: index === 0 ? folder : other, name: index === 0 ? 'First tab' : 'Second tab' }])))
      expect(outcomes.filter((outcome) => outcome._tag === 'Success')).toHaveLength(1)
      expect(outcomes.filter((outcome) => outcome._tag === 'Refused')).toMatchObject([{ reason: { _tag: 'Known', code: 'arrangement-exists' } }])
      expect(requests).toHaveLength(2)
      expect(new Set(requests.map((request) => request.parameters.subject))).toEqual(new Set([testSelection.subject]))
      for (const request of requests) expect(request.parameters.operations[0]).toEqual({ op: 'create', name: 'Sidebar' })
      expect(items).toHaveLength(1)
      const refusedIndex = outcomes.findIndex((outcome) => outcome._tag === 'Refused')
      const missingFolder = refusedIndex === 0 ? folder : other
      const refusedState = states[refusedIndex]!.at(-1)!
      expect(refusedState).toMatchObject({ phase: 'refused', retryReady: true, arrangement: items[0] })
      expect(refusalText(refusedState.refusal!)).toBe('The folder layout was created elsewhere; refresh and try again.')
      expect(items[0]?.body.folders[missingFolder]).toBeUndefined()
      expect(await editors[refusedIndex]!.retryEdit()).toEqual({ _tag: 'Success' })
      expect(requests).toHaveLength(3)
      expect(requests[2]?.parameters.operations.some((operation) => operation.op === 'create')).toBe(false)
      expect(requests[2]?.parameters.subject).toBe(testSelection.subject)
      expect(items).toHaveLength(1)
      for (const editor of editors) editor.accept({ owner: testSelection.owner, items })
      for (const state of states) {
        expect(state.at(-1)).toMatchObject({ phase: 'synced', arrangement: items[0] })
        expect(Object.keys(state.at(-1)?.arrangement?.body.folders ?? {}).sort()).toEqual([folder, other])
      }
    } finally {
      for (const editor of editors) editor.close()
    }
  })

  it('edits the legacy random-id Sidebar without creating a reserved Sidebar beside it', async () => {
    const legacy = { ...optimisticArrangement(testArrangement(), [create]), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
    const h = harness([legacy])
    try {
      expect(await h.editor.edit([rename])).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(1)
      expect(h.requests[0]?.parameters).toEqual({ owner: testSelection.owner, subject: legacy.id, operations: [rename] })
      expect(h.items()).toHaveLength(1)
      expect(h.last().arrangement?.id).toBe(legacy.id)
      expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Renamed')
    } finally {
      h.editor.close()
    }
  })

  it('refreshes a create race and offers an explicit edit of the winner without another create', async () => {
    const h = harness([])
    const gate = Promise.withResolvers<void>()
    h.gate(gate.promise)
    h.behavior(refused('arrangement-exists'))
    const first = h.editor.edit([create])
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    h.replace([testArrangement()])
    gate.resolve()
    expect(await first).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'arrangement-exists' } })
    await vi.waitFor(() => expect(h.last().retryReady).toBe(true))
    expect(h.requests).toHaveLength(1)
    expect(h.requests[0]?.parameters.subject).toBe(testSelection.subject)
    expect(h.last()).toMatchObject({ phase: 'refused', arrangement: testArrangement() })
    expect(h.last().arrangement?.body.folders[folder]).toBeUndefined()
    h.behavior(undefined)
    expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
    expect(h.requests[1]?.parameters).toEqual({ ...testSelection, operations: [create] })
    h.editor.close()
  })

  it('keeps Unknown explicit for transport, unknown codes and rejected results without reasons', async () => {
    expect(arrangementRefusal(new ActionTransportFailure({ cause: 'network', message: 'Connection lost.' }), [folder])).toMatchObject({ reason: { _tag: 'Unknown' }, detail: 'Connection lost.' })
    const unknown = arrangementRefusal(refused('future-code', 'A future rule refused it.'), [folder])
    expect(unknown.reason).toEqual({ _tag: 'Unknown', code: 'future-code' })
    expect(refusalText(unknown)).toBe('The change was not saved.')
    const h = harness()
    h.behavior('rejected')
    expect(await h.editor.edit([rename])).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Unknown' } })
    expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Inbox')
    h.editor.close()
  })

  it('preserves complete typed refusal envelopes without parsing their wording', () => {
    for (const code of ['arrangement-cycle', 'forbidden', 'invalid-arrangement-key', 'stale-fence', 'remote-unavailable']) {
      const failure = refused(code, 'Identical opaque detail')
      const value = arrangementRefusal(failure, ['agent/example', folder])
      expect(value).toEqual({ reason: { _tag: 'Known', code }, detail: failure.response.message, targets: ['agent/example', folder], error: failure.response })
    }
  })

  it.each([
    ['stale-fence', 'The folder layout changed elsewhere; refresh and try again.'],
    ['forbidden', 'This connection cannot edit the folder layout; request editing access.'],
    ['arrangement-owner-forbidden', 'This connection cannot edit the folder layout; request editing access.'],
    ['unsupported-capability', 'Folder editing is unavailable for this connection; reconnect with editing access.'],
    ['arrangement-exists', 'The folder layout was created elsewhere; refresh and try again.'],
    ['arrangement-folder-exists', 'This folder already exists; refresh before creating another folder.'],
    ['arrangement-retired', 'This folder layout was removed; refresh to use the current layout.'],
    ['arrangement-folder-deleted', 'This folder was deleted; refresh and choose another folder.'],
    ['arrangement-limit', 'The folder layout has reached its limit; reduce the layout before trying again.'],
    ['arrangement-body-too-large', 'The folder layout is too large; reduce the layout before trying again.'],
    ['arrangement-cycle', 'A folder cannot contain itself; choose a different parent folder.'],
    ['invalid-arrangement-subject', 'The folder layout could not be identified; refresh and try again.'],
    ['invalid-arrangement-action', 'This change could not be accepted; refresh and try again.'],
    ['invalid-arrangement-operations', 'This change could not be accepted; refresh and try again.'],
    ['invalid-arrangement-folder', 'The folder could not be identified; refresh and try again.'],
    ['invalid-arrangement-name', 'The folder name is not valid; use a shorter, nonblank name.'],
    ['invalid-arrangement-key', 'The folder order could not be saved; refresh and try again.'],
    ['invalid-subject-reference', 'The agent could not be placed; refresh and select it again.'],
    ['not-found', 'The folder layout or item no longer exists; refresh and choose another.'],
    ['validation-failed', 'This change could not be accepted; refresh and try again.'],
    ['idempotency-conflict', 'This change conflicts with an earlier change; refresh and try again.'],
    ['internal', 'The change was not saved.'],
    ['remote-unavailable', 'The change was not saved.'],
    ['future-code', 'The change was not saved.'],
  ])('uses a plain sentence for %s without exposing server detail', (code, sentence) => {
    const refusal = arrangementRefusal(refused(code, `Unknown ${code}: opaque server detail`), [folder])
    expect(refusalText(refusal)).toBe(sentence)
    expect(refusalText(refusal)).not.toContain(code)
    expect(refusalText(refusal)).not.toContain('Unknown')
    expect(refusal.detail).toBe(`Unknown ${code}: opaque server detail`)
  })

  it('uses the generic sentence when no typed refusal reason is available', () => {
    const refusal = arrangementRefusal(new ActionTransportFailure({ cause: 'network', message: 'Unknown: opaque transport detail' }), [folder])
    expect(refusal.reason).toEqual({ _tag: 'Unknown' })
    expect(refusalText(refusal)).toBe('The change was not saved.')
  })

  it('refetches a stale fence and rolls back, but submits nothing until explicit retry', async () => {
    const h = harness()
    h.behavior(refused('stale-fence', 'The graph fence changed.'))
    const result = await h.editor.edit([rename])
    expect(result).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'stale-fence' } })
    await vi.waitFor(() => expect(h.last().retryReady).toBe(true))
    expect(h.requests).toHaveLength(1)
    expect(h.counters()).toEqual({ snapshots: 2, reads: 2 })
    expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Inbox')
    h.behavior(undefined)
    expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
    expect(h.requests[1]?.fence.snapshot_id).toBe('snapshot/fresh/3')
    expect(h.requests[1]?.idempotency_key).toBe(h.requests[0]?.idempotency_key)
    h.editor.close()
  })

  it('replays an uncertain first creation with its exact identity, never a second Sidebar', async () => {
    const h = harness([])
    h.behavior(new ActionTransportFailure({ cause: 'network', message: 'Reply lost; delivery unknown.' }))
    await h.editor.edit([create])
    const original = h.requests[0]!
    h.behavior(undefined)
    expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
    expect(h.requests[1]).toMatchObject({ id: original.id, idempotency_key: original.idempotency_key, parameters: original.parameters })
    expect(h.items()).toHaveLength(1)
    h.editor.close()
  })

  it.each(['rename', 'remove', 'replace'] as const)('refuses an uncertain edit retry when the original Sidebar loses the winner by %s', async (transition) => {
    const h = harness([{ ...optimisticArrangement(testArrangement(), [create]), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }])
    try {
      h.behavior(new ActionTransportFailure({ cause: 'network', message: 'Reply lost; delivery unknown.' }))
      expect(await h.editor.edit([rename])).toMatchObject({ _tag: 'Refused' })
      const original = h.items()[0]!
      const next = transition === 'rename'
        ? [{ ...original, body: { ...original.body, name: { value: 'Archive', revision: 'claim/native-rename' } } }]
        : transition === 'remove' ? []
        : [{ ...original, id: 'arrangement/person/example/00000000-0000-7000-8000-000000000008' }]
      h.replace(next)
      h.behavior(undefined)
      const result = await h.editor.retryEdit()
      expect(result).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Unknown' }, targets: [folder] })
      if (result._tag === 'Refused') expect(refusalText(result)).toBe('The change was not saved.')
      expect(h.requests).toHaveLength(1)
      expect(h.items()).toEqual(next)
      expect(h.last().phase).toBe('refused')
      expect(h.last().arrangement).toEqual(transition === 'replace' ? next[0] : undefined)
    } finally {
      h.editor.close()
    }
  })

  it('does not treat an observed retired uncertain creation as a reserved unseen subject', async () => {
    const h = harness([])
    try {
      h.behavior(new ActionTransportFailure({ cause: 'network', message: 'Reply lost; delivery unknown.' }))
      await h.editor.edit([create])
      const subject = h.requests[0]!.parameters.subject
      const observed = { ...optimisticArrangement(testArrangement(), [create]), id: subject }
      h.replace([observed])
      const next: Arrangement[] = []
      h.replace(next)
      h.behavior(undefined)
      expect(await h.editor.retryEdit()).toMatchObject({ _tag: 'Refused' })
      expect(h.requests).toHaveLength(1)
      expect(h.items()).toEqual(next)
      expect(h.last().arrangement).toBeUndefined()
      expect(h.last().restoreUnavailable).toBe(true)
    } finally {
      h.editor.close()
    }
  })

  it('remembers an invisible retired reserved subject and never submits another create', async () => {
    const h = harness([], [testSelection.subject])
    try {
      expect(h.requests).toEqual([]) // Absence alone never creates automatically.
      expect(await h.editor.edit([create])).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'arrangement-retired' } })
      expect(h.last()).toMatchObject({ restoreUnavailable: true, retryReady: false })
      expect(h.last().arrangement).toBeUndefined()
      h.behavior(undefined)
      expect(await h.editor.retryEdit()).toMatchObject({ _tag: 'Refused' })
      expect(await h.editor.edit([{ ...create, id: other }])).toMatchObject({ _tag: 'Refused' })
      expect(h.requests).toHaveLength(1)
      expect(h.requests[0]?.parameters.subject).toBe(testSelection.subject)
      expect(h.items()).toEqual([])
    } finally {
      h.editor.close()
    }
  })

  it('keeps arrangement-exists without a visible winner transient and retries only explicitly', async () => {
    const h = harness([])
    try {
      h.behavior(refused('arrangement-exists'))
      expect(await h.editor.edit([create])).toMatchObject({ _tag: 'Refused' })
      expect(h.requests).toHaveLength(1)
      expect(h.last()).toMatchObject({ phase: 'refused', retryReady: true })
      expect(refusalText(h.last().refusal!)).toBe('The change was not saved.')
      expect(h.last().restoreUnavailable).not.toBe(true)
      expect(h.last().arrangement).toBeUndefined()
      h.behavior(undefined)
      expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(2)
      expect(h.requests[1]?.parameters.subject).toBe(testSelection.subject)
      expect(h.items()).toHaveLength(1)
    } finally { h.editor.close() }
  })

  it('keeps a live legacy Sidebar usable even after learning the reserved subject is retired', async () => {
    const h = harness([], [testSelection.subject])
    try {
      await h.editor.edit([create])
      const legacy = { ...optimisticArrangement(testArrangement(), [create]), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
      h.replace([legacy])
      expect(h.last().restoreUnavailable).not.toBe(true)
      expect(await h.editor.edit([rename])).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(2)
      expect(h.requests[1]?.parameters.subject).toBe(legacy.id)
      expect(h.requests[1]?.parameters.operations).toEqual([rename])
    } finally { h.editor.close() }
  })

  it.each(['arrangement-retired', 'forbidden', 'future-code', 'rejected'] as const)('rebuilds a definitely non-applied %s refusal on explicit retry against the adopted legacy Sidebar', async (code) => {
    const h = harness([], code === 'arrangement-retired' ? [testSelection.subject] : [])
    try {
      if (code !== 'arrangement-retired') h.behavior(code === 'rejected' ? 'rejected' : refused(code))
      const outcome = await h.editor.edit([create])
      expect(outcome).toMatchObject({ _tag: 'Refused' })
      if (code !== 'rejected') expect(outcome).toMatchObject({ error: { code } })
      expect(h.requests).toHaveLength(1)
      expect(h.items()).toEqual([])
      const original = h.requests[0]!
      const legacy = { ...testArrangement(), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
      h.replace([legacy])
      h.behavior(undefined)
      expect(h.last().retryReady).toBe(true)
      expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(2)
      expect(h.requests[1]?.parameters).toEqual({ owner: testSelection.owner, subject: legacy.id, operations: [create] })
      expect(h.requests[1]?.id).not.toBe(original.id)
      expect(h.requests[1]?.idempotency_key).not.toBe(original.idempotency_key)
      expect(await h.editor.edit([rename])).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(3)
      expect(h.items()).toHaveLength(1)
      expect(h.last().arrangement?.body.folders[folder]?.name.value).toBe('Renamed')
      expect(h.requests.flatMap((request) => request.parameters.operations).filter((operation) => operation.op === 'create')).toHaveLength(1)
      expect(h.requests.filter((request) => request.parameters.operations.some((operation) => operation.op === 'create')).map((request) => request.parameters.subject)).toEqual([testSelection.subject])
    } finally { h.editor.close() }
  })

  it('permits a new explicit edit after refusing an uncertain retry whose original subject is no longer adopted', async () => {
    const legacy = { ...optimisticArrangement(testArrangement(), [create]), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }
    const h = harness([legacy])
    try {
      h.behavior(new ActionTransportFailure({ cause: 'network', message: 'Reply lost; delivery unknown.' }))
      await h.editor.edit([rename])
      h.replace([{ ...legacy, body: { ...legacy.body, name: { value: 'Archive', revision: 'claim/native-rename' } } }])
      h.behavior(undefined)
      expect(await h.editor.retryEdit()).toMatchObject({ _tag: 'Refused' })
      expect(h.requests).toHaveLength(1)
      expect(await h.editor.edit([{ ...create, id: other }])).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(2)
      expect(h.requests[1]?.parameters.operations).toEqual([{ op: 'create', name: 'Sidebar' }, { ...create, id: other }])
      expect(h.requests[1]?.parameters.subject).toBe(testSelection.subject)
      expect(h.items().find((item) => item.id === legacy.id)?.body.folders[folder]?.name.value).toBe('Inbox')
    } finally { h.editor.close() }
  })

  it('does not mistake a snapshot refusal for proof that an earlier uncertain action was unapplied', async () => {
    const h = harness()
    try {
      h.behavior(new ActionTransportFailure({ cause: 'network', message: 'Reply lost; delivery unknown.' }))
      await h.editor.edit([rename])
      h.replace([{ ...optimisticArrangement(testArrangement(), [create]), id: 'arrangement/person/example/00000000-0000-7000-8000-000000000009' }])
      h.behavior(undefined)
      h.snapshotFailure(refused('forbidden'))
      expect(await h.editor.retryEdit()).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'forbidden' } })
      h.snapshotFailure(undefined)
      expect(await h.editor.retryEdit()).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Unknown' } })
      expect(h.requests).toHaveLength(1)
      expect(h.items()[0]?.body.folders[folder]?.name.value).toBe('Inbox')
    } finally { h.editor.close() }
  })

  it('does not write before a usable fresh snapshot, and cancels a queued edit after refusal', async () => {
    const h = harness([])
    h.snapshotFailure(new ActionTransportFailure({ cause: 'network', message: 'Snapshot unavailable.' }))
    const first = h.editor.edit([create])
    const second = h.editor.edit([{ ...create, id: other }])
    expect((await first)._tag).toBe('Refused')
    expect((await second)._tag).toBe('Refused')
    expect(h.requests).toEqual([])
    expect(h.last().arrangement).toBeUndefined()
    h.editor.close()
  })

  it('drops optimistic state and aborts in-flight work when the editor closes', async () => {
    const h = harness()
    const gate = Promise.withResolvers<void>()
    h.gate(gate.promise)
    const pending = h.editor.edit([rename])
    await vi.waitFor(() => expect(h.requests).toHaveLength(1))
    h.editor.close()
    const count = h.states.length
    expect((await pending)._tag).toBe('Refused')
    gate.resolve()
    await Promise.resolve()
    expect(h.states).toHaveLength(count)
  })
})

it('uses the SDK HTTP action port and fresh snapshots while retaining native error codes', async () => {
  const requests: { readonly url: string; readonly body?: unknown }[] = []
  let reads = 0
  const error: ErrorEnvelope = refused('arrangement-cycle', 'A cycle is not allowed.').response
  const client = new St3Client({ baseUrl: 'https://gateway.example.test', fetchImpl: async (input, init) => {
    requests.push({ url: String(input), ...(typeof init?.body === 'string' ? { body: JSON.parse(init.body) as unknown } : {}) })
    if (init?.method === 'POST') return Response.json(error, { status: 409 })
    reads++
    return Response.json({ api_version: 'st3.client.v0', request_id: 'request/example', snapshot: { ...testSnapshot, id: `snapshot/fresh/${reads}` }, value: testCapabilities })
  } })
  const port = arrangementActions(client)
  expect(await Effect.runPromise(port.snapshot)).toBe('snapshot/fresh/1')
  expect(await Effect.runPromise(port.snapshot)).toBe('snapshot/fresh/2')
  const action: ActionOf<'arrangement.edit'> = { api_version: 'st3.client.v0', type: 'arrangement.edit', id: 'action/example', idempotency_key: 'arrangement-edit-example-key', parameters: { ...testSelection, operations: [rename] }, fence: { snapshot_id: 'snapshot/fresh/2', subject_revisions: {} } }
  const outcome = await Effect.runPromise(Effect.result(port.submitAction(action)))
  expect(outcome).toMatchObject({ _tag: 'Failure', failure: { _tag: 'ActionRefused', response: error, status: 409 } })
  expect(requests[2]).toEqual({ url: 'https://gateway.example.test/v1/client/actions', body: action })
})

it('does not create again when an acknowledged creation is missing from the inventory', async () => {
  const h = harness([])
  h.hideAccepted()
  const first = h.editor.edit([create])
  const second = h.editor.edit([{ ...create, id: other }])
  expect(await first).toEqual({ _tag: 'Success' })
  expect(await second).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Unknown' } })
  expect(h.requests).toHaveLength(1)
  h.editor.close()
})

it('keeps a failed stale-fence refresh explicit and refreshes again on user retry', async () => {
  const h = harness()
  const gate = Promise.withResolvers<void>()
  h.gate(gate.promise)
  h.behavior(refused('stale-fence'))
  const pending = h.editor.edit([rename])
  await vi.waitFor(() => expect(h.requests).toHaveLength(1))
  h.snapshotFailure(new ActionTransportFailure({ cause: 'network', message: 'Fresh snapshot unavailable.' }))
  gate.resolve()
  await pending
  await vi.waitFor(() => expect(h.counters().snapshots).toBe(2))
  expect(h.last().retryReady).toBe(false)
  expect(h.requests).toHaveLength(1)
  h.snapshotFailure(undefined)
  h.behavior(undefined)
  expect(await h.editor.retryEdit()).toEqual({ _tag: 'Success' })
  expect(h.requests[1]?.fence.snapshot_id).toBe('snapshot/fresh/4')
  h.editor.close()
})

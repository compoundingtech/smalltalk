import type { Arrangement } from '@smalltalk/st3-client'
import { ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { ActionRefused } from '@st3/sdk/effect'
import { Effect } from 'effect'
import { describe, expect, it } from 'vitest'
import { arrangementSidebarDoc } from './client.ts'
import { project } from './core.mts'
import { createArrangementEditor, optimisticArrangement, type ArrangementEditorState, type SidebarOperation } from './edit.ts'
import { testArrangement, testSelection, testSnapshot } from './testGateway.ts'

const alpha = 'agent/root-alpha'
const beta = 'agent/root-beta'
const deletedFolder = '00000000-0000-7000-8000-000000000002'
const place = (subject: string, key: string): SidebarOperation => ({ op: 'subject.place', subject, folder: null, key })
const roots = (arrangement: Arrangement, live: readonly string[]) =>
  project(arrangementSidebarDoc(arrangement), live, { rootOrder: 'placement' }).unfiled

const harness = (initial: Arrangement) => {
  let authoritative = initial
  let state: ArrangementEditorState = { arrangement: initial, phase: 'synced', retryReady: false }
  let refuse = false
  let reads = 0
  let accepted = 0
  const editor = createArrangementEditor({
    owner: testSelection.owner,
    read: async () => { reads++; return { owner: testSelection.owner, items: [authoritative] } },
    actions: {
      snapshot: Effect.succeed(testSnapshot.id),
      submitAction: (request) => Effect.gen(function* () {
        if (refuse) return yield* Effect.fail(new ActionRefused({
          status: 403,
          message: 'Synthetic reorder denied.',
          response: { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/root-order', code: 'forbidden', message: 'Synthetic reorder denied.', retryable: false, details: {} },
        }))
        const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
        accepted++
        authoritative = { ...optimisticArrangement(authoritative, operations), revision: `claim/root-order-${accepted}` }
        return decodeUnknownSync(ActionResult)({
          kind: 'action-result', action_id: request.id, operation_id: 'operation/root-order', snapshot_id: testSnapshot.id,
          status: 'completed', affected_ids: [request.parameters.subject], arrangement_revision: authoritative.revision,
        })
      }),
    },
    onState: (next) => { state = next },
  })
  editor.accept({ owner: testSelection.owner, items: [authoritative] })
  return { editor, state: () => state, authoritative: () => authoritative, reads: () => reads, refuse: () => { refuse = true } }
}

describe('arrangement-aware root placement ordering', () => {
  it.each([
    { direction: 'up', live: [alpha, beta], moved: beta, key: 'Zz', expected: [beta, alpha], rollbackKey: 'a2' },
    { direction: 'down', live: [beta, alpha], moved: beta, key: 'a2', expected: [alpha, beta], rollbackKey: 'Zz' },
  ])('reorders two unfiled agents $direction immediately, retains the success reread, and rolls back refusal', async ({ live, moved, key, expected, rollbackKey }) => {
    const initial = optimisticArrangement(testArrangement(), live.map((subject, index) => place(subject, index === 0 ? 'a0' : 'a1')))
    const h = harness(initial)
    const shown = () => roots(h.state().arrangement ?? testArrangement(), live)
    try {
      expect(shown()).toEqual(live)
      const pending = h.editor.edit([place(moved, key)])
      expect(h.state().phase).toBe('pending')
      expect(shown()).toEqual(expected)
      expect(await pending).toEqual({ _tag: 'Success' })
      expect(h.reads()).toBe(2)
      expect(h.state().phase).toBe('synced')
      expect(h.state().arrangement).toEqual(h.authoritative())
      expect(h.state().arrangement?.revision).toBe('claim/root-order-1')
      expect(shown()).toEqual(expected)

      h.refuse()
      const refused = h.editor.edit([place(moved, rollbackKey)])
      expect(h.state().phase).toBe('pending')
      expect(shown()).toEqual(live)
      expect(await refused).toMatchObject({ _tag: 'Refused', reason: { _tag: 'Known', code: 'forbidden' }, targets: [moved] })
      expect(h.state().phase).toBe('refused')
      expect(h.state().arrangement).toEqual(h.authoritative())
      expect(shown()).toEqual(expected)
    } finally {
      h.editor.close()
    }
  })

  it('orders a placement lifted from a deleted folder by its retained key before absent placements', () => {
    const absentAlpha = 'agent/absent-alpha'
    const absentBeta = 'agent/absent-beta'
    const arrangement = optimisticArrangement(testArrangement(), [
      { op: 'folder.create', id: deletedFolder, name: 'Synthetic deleted folder', parent: null, key: 'a0' },
      { op: 'subject.place', subject: beta, folder: deletedFolder, key: 'Zz' },
      place(alpha, 'a0'),
      { op: 'folder.delete', id: deletedFolder },
    ])
    const live = [absentBeta, alpha, absentAlpha, beta]
    const projected = project(arrangementSidebarDoc(arrangement), live, { rootOrder: 'placement' })
    expect(projected.folders).toEqual([])
    expect(projected.unfiled).toEqual([beta, alpha, absentAlpha, absentBeta])
    expect(roots(arrangement, [...live].reverse())).toEqual(projected.unfiled)
  })

  it('breaks equal root placement keys by subject rather than native register or caller insertion order', () => {
    const arrangement = optimisticArrangement(testArrangement(), [place(beta, 'a0'), place(alpha, 'a0')])
    expect(roots(arrangement, [beta, alpha])).toEqual([alpha, beta])
    expect(roots(arrangement, [alpha, beta])).toEqual([alpha, beta])
  })
})

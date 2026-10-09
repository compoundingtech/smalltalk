import type { ActionOf, Arrangement } from '@smalltalk/st3-client'
import { ActionResult, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { Effect } from 'effect'
import { describe, expect, it } from 'vitest'
import { createArrangementEditor, optimisticArrangement, type ArrangementEditorState, type SidebarOperation } from './edit.ts'
import { testArrangement, testSelection, testSnapshot } from './testGateway.ts'

const firstFolder = '00000000-0000-7000-8000-000000000002'
const nextFolder = '00000000-0000-7000-8000-000000000003'

const harness = () => {
  let items: Arrangement[] = []
  const requests: ActionOf<'arrangement.edit'>[] = []
  const states: ArrangementEditorState[] = []
  const editor = createArrangementEditor({
    owner: testSelection.owner,
    read: async () => ({ owner: testSelection.owner, items }),
    actions: {
      snapshot: Effect.succeed(testSnapshot.id),
      submitAction: (request) => Effect.sync(() => {
        requests.push(structuredClone(request))
        const existing = items.find((item) => item.id === request.parameters.subject)
        const base = existing ?? { ...testArrangement(), id: request.parameters.subject }
        const operations = request.parameters.operations.filter((operation): operation is SidebarOperation => operation.op.startsWith('folder.') || operation.op === 'subject.place')
        const revision = `claim/accepted-${requests.length}`
        items = [...items.filter((item) => item.id !== base.id), { ...optimisticArrangement(base, operations), revision }]
        return decodeUnknownSync(ActionResult)({
          kind: 'action-result', action_id: request.id, operation_id: `operation/lifecycle-${requests.length}`,
          snapshot_id: testSnapshot.id, status: 'completed', affected_ids: [base.id], arrangement_revision: revision,
        })
      }),
    },
    onState: (state) => states.push(state),
  })
  editor.accept({ owner: testSelection.owner, items })
  return { editor, requests, states, items: () => items, replace: (next: Arrangement[]) => { items = next; editor.accept({ owner: testSelection.owner, items }) } }
}

const createFirst = [{ op: 'folder.create', id: firstFolder, name: 'First', parent: null, key: 'a0' }] as const
const createNext = [{ op: 'folder.create', id: nextFolder, name: 'Next', parent: null, key: 'a1' }] as const

describe('observed reserved Sidebar lifecycle', () => {
  it('creates the reserved subject only on a user edit and keeps its identity after a native rename', async () => {
    const h = harness()
    try {
      expect(h.requests).toEqual([])
      expect(await h.editor.edit(createFirst)).toEqual({ _tag: 'Success' })
      const original = h.items()[0]!
      expect(original.id).toBe(testSelection.subject)
      expect(h.requests[0]?.parameters.operations).toEqual([{ op: 'create', name: 'Sidebar' }, ...createFirst])
      const renamed = { ...original, revision: 'claim/native-rename', body: { ...original.body, name: { value: 'Archive', revision: 'claim/native-rename' } } }
      h.replace([renamed])
      expect(h.states.at(-1)?.arrangement).toEqual(renamed)
      expect(await h.editor.edit(createNext)).toEqual({ _tag: 'Success' })
      expect(h.requests).toHaveLength(2)
      expect(h.requests[1]?.parameters).toEqual({ owner: testSelection.owner, subject: original.id, operations: createNext })
      expect(h.requests[1]?.fence.subject_revisions).toEqual({ [original.id]: 'claim/native-rename' })
      expect(h.items()).toHaveLength(1)
      expect(h.states.at(-1)?.arrangement?.body.name.value).toBe('Archive')
      expect(Object.keys(h.states.at(-1)?.arrangement?.body.folders ?? {})).toEqual([firstFolder, nextFolder])
    } finally { h.editor.close() }
  })

  it('remembers a reserved subject observed live then retired and never creates another Sidebar', async () => {
    const h = harness()
    try {
      expect(await h.editor.edit(createFirst)).toEqual({ _tag: 'Success' })
      h.replace([])
      expect(h.states.at(-1)).toMatchObject({ phase: 'synced', restoreUnavailable: true, sidebarSubject: testSelection.subject })
      expect(h.states.at(-1)?.arrangement).toBeUndefined()
      expect(await h.editor.edit(createNext)).toMatchObject({ _tag: 'Refused' })
      expect(h.requests).toHaveLength(1)
      expect(h.items()).toEqual([])
    } finally { h.editor.close() }
  })
})

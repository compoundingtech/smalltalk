import * as Atom from 'effect/reactivity/Atom'

import type { Arrangement } from '@smalltalk/st3-client'
import { arrangementSidebarDoc, type FolderState } from './client.ts'
import { optimisticArrangement, type EditOutcome, type SidebarOperation } from './edit.ts'
import { emptyDoc } from './core.mts'

/** Story registries own isolated previews; fixture edits never reach the person's sidebar. */
export const fixtureFolders = Atom.keepAlive(
  Atom.make((get): FolderState => {
    let arrangement: Arrangement = {
      id: 'arrangement/person/fixture/00000000-0000-7000-8000-000000000001', owner: 'person/fixture',
      kind: 'arrangement', deleted: false, revision: 'claim/fixture', updated_at: '2026-01-01T00:00:00Z',
      body: { version: 1, name: { value: 'Sidebar', revision: 'claim/fixture' }, folders: {}, placements: {} },
    }
    const edit = (operations: readonly SidebarOperation[]): Promise<EditOutcome> => {
      arrangement = optimisticArrangement(arrangement, operations)
      get.setSelf({ doc: arrangementSidebarDoc(arrangement), phase: 'fixture', edit })
      return Promise.resolve({ _tag: 'Success' })
    }
    return { doc: emptyDoc(), phase: 'fixture', edit }
  }),
)

import * as Atom from 'effect/reactivity/Atom'

import type { FolderState } from './client.ts'
import { applyOp, emptyDoc, type FolderOp } from './core.mts'

/** Story registries own isolated replicas; fixture edits never reach the person's sidebar. */
export const fixtureFolders = Atom.keepAlive(
  Atom.make((get): FolderState => {
    const doc = emptyDoc()
    const edit = (ops: readonly FolderOp[]) => {
      for (const op of ops) applyOp(doc, op)
      get.setSelf({ doc: structuredClone(doc), phase: 'fixture', edit })
    }
    return { doc, phase: 'fixture', edit }
  }),
)

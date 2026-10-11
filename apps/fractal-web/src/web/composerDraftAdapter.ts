import { createComposerDraftSession, type ComposerDraftSession, type ComposerDraftStore } from './composerDrafts.ts'

/** Structural kit boundary; text revisions include the runtime's optimistic submit clear. */
export interface ComposerDraftSnapshot {
  readonly text: string
  readonly revision: number
  readonly savedAt: number
  readonly cause: 'user' | 'submit-reset' | 'restore' | 'send-failed-restore'
}
export interface ComposerDraftRestore {
  readonly text: string
  readonly savedAt: number
  readonly expectedRevision: number
}
export interface EmbraceComposerHandle {
  readonly getDraft: () => ComposerDraftSnapshot
  readonly restoreDraft: (draft: ComposerDraftRestore) => boolean
}
export type ComposerSendOutcome = { readonly _tag: 'Confirmed' } | { readonly _tag: 'Unconfirmed' }

/** The kit owns editing; this seam owns persistence and acknowledgement, never DOM mutation. */
export const createComposerDraftAdapter = ({ store, conversationId, getHandle, now }: {
  readonly store: ComposerDraftStore
  readonly conversationId: string
  readonly getHandle: () => EmbraceComposerHandle | undefined
  readonly now?: () => number
}): ComposerDraftAdapter => {
  let session: ComposerDraftSession | undefined
  let handlingChange = false
  let restoring = false
  const restore = () => {
    const handle = getHandle()
    if (handle === undefined || handlingChange || restoring) return false
    // Capture BEFORE the initial store read. Kit revision checks remain authoritative if it
    // changes while loading; a future asynchronous adapter must keep this ordering intact.
    const expectedRevision = handle.getDraft().revision
    const current = getSession().getSnapshot()
    restoring = true
    try {
      return handle.restoreDraft({ text: current.text, savedAt: current.updatedAt, expectedRevision })
    } finally { restoring = false }
  }
  const getSession = () => {
    if (session === undefined) {
      session = createComposerDraftSession({ store, conversationId, ...(now === undefined ? {} : { now }) })
      session.subscribe(() => { restore() })
    }
    return session
  }
  return {
    restore,
    onDraftChange: (snapshot) => {
      // Runtime clears/restores carry revisions but never acknowledge a send.
      if (restoring || snapshot.cause !== 'user') return
      handlingChange = true
      try { getSession().edit(snapshot.text) } finally { handlingChange = false }
    },
    onSending: (content) => {
      const confirmed = getSession().beginSend(content)
      let settled = false
      return (outcome) => {
        if (settled) return
        settled = true
        if (outcome._tag === 'Confirmed') confirmed()
      }
    },
    close: () => { session?.close(); session = undefined },
  }
}

export interface ComposerDraftAdapter {
  /** Invoke after the kit ref attaches. Also runs for newer storage events on untouched sessions. */
  readonly restore: () => boolean
  readonly onDraftChange: (snapshot: ComposerDraftSnapshot) => void
  readonly onSending: (content: string) => (outcome: ComposerSendOutcome) => void
  readonly close: () => void
}

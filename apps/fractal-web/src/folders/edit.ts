import type { ActionOf, Arrangement, ArrangementOperation, ErrorEnvelope } from '@smalltalk/st3-client'
import { ArrangementEditParameters, ErrorCode, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import type { ActionFailure, ArrangementActionPort } from '@st3/sdk/effect'
import { Effect, Schema } from 'effect'
import { reservedSidebarSubject, sidebarCandidates, sidebarWinner, type ArrangementInventory } from '../data/arrangements.ts'

/** Only folder/placement operations are user-editable: Sidebar name and retirement are not. */
export type SidebarOperation = Extract<ArrangementOperation, { op: 'folder.create' | 'folder.rename' | 'folder.move' | 'folder.delete' | 'subject.place' }>
type Fields<TOp extends SidebarOperation['op']> = Omit<Extract<SidebarOperation, { op: TOp }>, 'op'>
export const sidebarOperations = {
  createFolder: (fields: Fields<'folder.create'>): SidebarOperation => ({ ...fields, op: 'folder.create' }),
  renameFolder: (fields: Fields<'folder.rename'>): SidebarOperation => ({ ...fields, op: 'folder.rename' }),
  moveFolder: (fields: Fields<'folder.move'>): SidebarOperation => ({ ...fields, op: 'folder.move' }),
  deleteFolder: (fields: Fields<'folder.delete'>): SidebarOperation => ({ ...fields, op: 'folder.delete' }),
  placeSubject: (fields: Fields<'subject.place'>): SidebarOperation => ({ ...fields, op: 'subject.place' }),
}

/** Lowercase UUIDv7 for new folders, generated only in memory. */
export const arrangementUuid = (): string => {
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  let time = BigInt(Date.now())
  for (let index = 5; index >= 0; index--) { bytes[index] = Number(time & 255n); time >>= 8n }
  bytes[6] = (bytes[6]! & 15) | 112
  bytes[8] = (bytes[8]! & 63) | 128
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

export interface ArrangementAttempt {
  readonly id: string
  readonly label: string
  readonly kind: 'folder' | 'agent'
}
export type ArrangementRefusal = {
  readonly reason: { readonly _tag: 'Known'; readonly code: Extract<typeof ErrorCode.Type, string> } | { readonly _tag: 'Unknown'; readonly code?: string }
  readonly detail: string
  readonly targets: readonly string[]
  readonly error?: ErrorEnvelope
  /** Admission found the subject, but its live inventory row is not visible yet. */
  readonly awaitingVisibility?: boolean
  /** Ephemeral request labels survive rollback, deletion and view filtering. */
  readonly attemptedTargets?: readonly ArrangementAttempt[]
}
export const arrangementRefusal = (failure: ActionFailure, targets: readonly string[]): ArrangementRefusal => {
  if (failure._tag !== 'ActionRefused') return { reason: { _tag: 'Unknown' }, detail: failure.message, targets }
  const code = decodeUnknownSync(ErrorCode)(failure.response.code)
  return {
    reason: typeof code === 'string' ? { _tag: 'Known', code } : { _tag: 'Unknown', code: code.raw },
    detail: failure.response.message, targets, error: failure.response,
  }
}
/** Diagnostic envelopes stay structured; the visible sentence never quotes server detail. */
const refusalSentences: Readonly<Partial<Record<Extract<typeof ErrorCode.Type, string>, string>>> = {
  'stale-fence': 'The folder layout changed elsewhere; refresh and try again.',
  'forbidden': 'This connection cannot edit the folder layout; request editing access.',
  'arrangement-owner-forbidden': 'This connection cannot edit the folder layout; request editing access.',
  'unsupported-capability': 'Folder editing is unavailable for this connection; reconnect with editing access.',
  'arrangement-exists': 'The folder layout was created elsewhere; refresh and try again.',
  'arrangement-folder-exists': 'This folder already exists; refresh before creating another folder.',
  'arrangement-retired': 'This folder layout was removed; refresh to use the current layout.',
  'arrangement-folder-deleted': 'This folder was deleted; refresh and choose another folder.',
  'arrangement-limit': 'The folder layout has reached its limit; reduce the layout before trying again.',
  'arrangement-body-too-large': 'The folder layout is too large; reduce the layout before trying again.',
  'arrangement-cycle': 'A folder cannot contain itself; choose a different parent folder.',
  'invalid-arrangement-subject': 'The folder layout could not be identified; refresh and try again.',
  'invalid-arrangement-action': 'This change could not be accepted; refresh and try again.',
  'invalid-arrangement-operations': 'This change could not be accepted; refresh and try again.',
  'invalid-arrangement-folder': 'The folder could not be identified; refresh and try again.',
  'invalid-arrangement-name': 'The folder name is not valid; use a shorter, nonblank name.',
  'invalid-arrangement-key': 'The folder order could not be saved; refresh and try again.',
  'invalid-subject-reference': 'The agent could not be placed; refresh and select it again.',
  'not-found': 'The folder layout or item no longer exists; refresh and choose another.',
  'validation-failed': 'This change could not be accepted; refresh and try again.',
  'idempotency-conflict': 'This change conflicts with an earlier change; refresh and try again.',
}
export const refusalText = (refusal: ArrangementRefusal): string =>
  (refusal.awaitingVisibility ? undefined : refusal.reason._tag === 'Known' ? refusalSentences[refusal.reason.code] : undefined) ?? 'The change was not saved.'

const optimisticRevision = 'claim/optimistic'
export const optimisticArrangement = (base: Arrangement, operations: readonly SidebarOperation[]): Arrangement => {
  const body = structuredClone(base.body)
  for (const operation of operations) {
    switch (operation.op) {
      case 'folder.create':
        body.folders[operation.id] = { name: { value: operation.name, revision: optimisticRevision }, position: { value: { parent: operation.parent, key: operation.key }, revision: optimisticRevision }, tombstone: null }
        break
      case 'folder.rename': {
        const folder = body.folders[operation.id]
        if (folder !== undefined) folder.name = { value: operation.name, revision: optimisticRevision }
        break
      }
      case 'folder.move': {
        const folder = body.folders[operation.id]
        if (folder !== undefined) folder.position = { value: { parent: operation.parent, key: operation.key }, revision: optimisticRevision }
        break
      }
      case 'folder.delete': {
        const folder = body.folders[operation.id]
        if (folder !== undefined) folder.tombstone = { value: true, revision: optimisticRevision }
        break
      }
      case 'subject.place':
        body.placements[operation.subject] = { value: { folder: operation.folder, key: operation.key }, revision: optimisticRevision }
        break
    }
  }
  // Server-resolved locations describe the old registers, not this optimistic layout.
  const { resolved: _resolved, ...header } = base
  return { ...header, body }
}
const emptyArrangement = (owner: string, id: string): Arrangement => ({
  id, owner, kind: 'arrangement', deleted: false, revision: optimisticRevision, updated_at: new Date().toISOString(),
  body: { version: 1, name: { value: 'Sidebar', revision: optimisticRevision }, folders: {}, placements: {} },
})
export interface ArrangementEditorState {
  readonly arrangement?: Arrangement
  readonly phase: 'synced' | 'pending' | 'refused'
  readonly refusal?: ArrangementRefusal
  readonly retryReady: boolean
  readonly sidebarCandidates?: readonly { readonly id: string; readonly label: string }[]
  readonly sidebarSubject?: string
  readonly restoreUnavailable?: boolean
}
export type EditOutcome = { readonly _tag: 'Success' } | ({ readonly _tag: 'Refused' } & ArrangementRefusal)
interface EditJob {
  readonly operations: readonly SidebarOperation[]
  readonly id: string
  readonly key: string
  readonly done: PromiseWithResolvers<EditOutcome>
  request?: ActionOf<'arrangement.edit'>
  /** The last submitted request was authoritatively refused, not transport-uncertain. */
  requestRefused?: boolean
  creationObserved?: boolean
}
export interface ArrangementEditor {
  readonly accept: (inventory: ArrangementInventory) => void
  readonly edit: (operations: readonly SidebarOperation[]) => Promise<EditOutcome>
  readonly retryEdit: () => Promise<EditOutcome>
  readonly close: () => void
}

/** One owner, one serialized queue, one reserved creation subject. No client persistence. */
export const createArrangementEditor = ({ owner, actions, read, onState }: {
  readonly owner: string
  readonly actions: ArrangementActionPort
  readonly read: (signal: AbortSignal) => Promise<ArrangementInventory>
  readonly onState: (state: ArrangementEditorState) => void
}): ArrangementEditor => {
  let inventory: ArrangementInventory | undefined
  const reservation = reservedSidebarSubject(owner)
  let reservedObserved = false
  let reservedRetired = false
  let creation: { readonly subject: string; readonly phase: 'acknowledged' | 'observed' } | undefined
  let closed = false
  let running = false
  let refusal: ArrangementRefusal | undefined
  let failed: EditJob | undefined
  let retryReady = false
  const prepareRetry = (job: EditJob): EditJob => {
    const winner = inventory === undefined ? undefined : sidebarWinner(inventory.items, owner)
    const request = job.request
    if (request === undefined || (request.parameters.subject === (winner?.id ?? reservation) &&
      request.parameters.operations.some((operation) => operation.op === 'create') === (winner === undefined))) return job
    // Changed parameters after a known non-applied refusal need a new action identity/key.
    const { request: _request, ...fields } = job
    return { ...fields, id: `action/${crypto.randomUUID()}`, key: crypto.randomUUID() }
  }
  const queue: EditJob[] = []
  const abort = new AbortController()
  const targets = (job: EditJob) => [...new Set(job.operations.flatMap((operation) => operation.op === 'subject.place'
    ? [operation.subject, ...(operation.folder === null ? [] : [operation.folder])]
    : [operation.id]))]
  const unknown = (detail: string, job: EditJob): ArrangementRefusal => ({ reason: { _tag: 'Unknown' }, detail, targets: targets(job) })
  const publish = () => {
    if (closed) return
    let arrangement = inventory === undefined ? undefined : sidebarWinner(inventory.items, owner)
    for (const job of queue) {
      arrangement ??= emptyArrangement(owner, reservation)
      arrangement = optimisticArrangement(arrangement, job.operations)
    }
    const candidates = inventory === undefined || arrangement?.id === reservation ? [] : sidebarCandidates(inventory.items, owner)
    const selection = {
      ...(candidates.length > 1 ? { sidebarCandidates: candidates.map((item) => ({ id: item.id, label: `${item.body.name.value} (${item.id})` })) } : {}),
      ...(arrangement === undefined && !reservedRetired ? {} : { sidebarSubject: arrangement?.id ?? reservation }),
      ...(reservedRetired && arrangement === undefined ? { restoreUnavailable: true } : {}),
    }
    onState({ ...selection, ...(arrangement === undefined ? {} : { arrangement }), phase: queue.length > 0 ? 'pending' : refusal === undefined ? 'synced' : 'refused', ...(refusal === undefined ? {} : { refusal }), retryReady: !selection.restoreUnavailable && retryReady })
  }
  const accept = (next: ArrangementInventory) => {
    if (closed || next.owner !== owner) return
    const previous = inventory?.snapshot
    const incoming = next.snapshot
    if (previous !== undefined && incoming !== undefined &&
      previous.host_id === incoming.host_id && previous.projection_version === incoming.projection_version &&
      incoming.store_index < previous.store_index) return
    const reserved = next.items.find((item) => item.id === reservation)
    if (reserved !== undefined && !reserved.deleted) { reservedObserved = true; reservedRetired = false }
    else if (reservedObserved || reserved?.deleted) reservedRetired = true
    for (const job of failed === undefined ? queue : [failed, ...queue]) {
      if (job.request?.parameters.operations.some((operation) => operation.op === 'create') &&
        next.items.some((item) => item.id === job.request?.parameters.subject)) job.creationObserved = true
    }
    if (creation !== undefined) {
      const subject = creation.subject
      const observed = next.items.find((item) => item.id === subject)
      if (observed !== undefined) creation = { subject, phase: 'observed' }
      if (creation.phase === 'observed' && (observed === undefined || observed.deleted)) {
        // A renamed reserved subject keeps its identity; retirement is terminal.
        creation = undefined
      }
    }
    inventory = next
    publish()
  }
  const fail = (job: EditJob, reason: ArrangementRefusal, waitForRefresh = false) => {
    failed = job
    const winner = inventory === undefined ? undefined : sidebarWinner(inventory.items, owner)
    const attempts = job.operations.map((operation): ArrangementAttempt => operation.op === 'subject.place'
      ? { id: operation.subject, label: operation.subject, kind: 'agent' }
      : {
        id: operation.id, kind: 'folder',
        label: operation.op === 'folder.create' || operation.op === 'folder.rename'
          ? operation.name : winner?.body.folders[operation.id]?.name.value ?? operation.id,
      })
    refusal = { ...reason, attemptedTargets: [...new Map(attempts.map((attempt) => [attempt.id, attempt])).values()] }
    retryReady = reason.error?.code !== 'stale-fence' && reason.error?.code !== 'arrangement-exists'
    const abandoned = queue.splice(0)
    publish() // Remove every optimistic edit before returning the refusal.
    if (!waitForRefresh) job.done.resolve({ _tag: 'Refused', ...reason })
    for (const later of abandoned) if (later !== job)
      later.done.resolve({ _tag: 'Refused', ...unknown('Not submitted because an earlier arrangement edit failed.', later) })
  }
  const refresh = async () => {
    const snapshot = await Effect.runPromise(Effect.result(actions.snapshot), { signal: abort.signal })
    if (snapshot._tag === 'Failure') return snapshot
    accept(await read(abort.signal))
    return snapshot
  }
  const drain = async () => {
    if (running || closed || failed !== undefined) return
    running = true
    try {
      while (queue.length > 0 && !closed) {
        let job = queue[0]!
        const fresh = await refresh()
        if (closed) return
        if (fresh._tag === 'Failure') { fail(job, arrangementRefusal(fresh.failure, targets(job))); return }
        const winner = inventory === undefined ? undefined : sidebarWinner(inventory.items, owner)
        if (winner === undefined && reservedRetired) {
          fail(job, unknown('Restoring a removed Sidebar is not available yet.', job))
          return
        }
        // Retarget only a definitive non-applied refusal, after the fresh inventory is accepted.
        if (job.requestRefused) { job = prepareRetry(job); queue[0] = job }
        if (job.request === undefined) {
          if (winner === undefined && creation?.phase === 'acknowledged') {
            fail(job, unknown('The created Sidebar is not visible in the complete inventory yet. Refresh before retrying.', job))
            return
          }
          const parameters = { owner, subject: winner?.id ?? reservation, operations: winner === undefined ? [{ op: 'create', name: 'Sidebar' } as const, ...job.operations] : [...job.operations] }
          if (!Schema.is(Schema.toEncoded(ArrangementEditParameters))(parameters)) {
            fail(job, unknown('The edit does not match the generated arrangement operation contract.', job))
            return
          }
          job.request = { api_version: 'st3.client.v0', type: 'arrangement.edit', id: job.id, idempotency_key: job.key, parameters, fence: { snapshot_id: fresh.success, subject_revisions: winner === undefined ? {} : { [winner.id]: winner.revision } } }
        } else {
          // Unknown delivery retries must replay exactly the original identity and parameters.
          const unobservedCreation = job.request.parameters.operations.some((operation) => operation.op === 'create') && !job.creationObserved
          if (winner?.id !== job.request.parameters.subject && !(winner === undefined && unobservedCreation)) {
            fail(job, unknown('The folder layout changed elsewhere; refresh and try again.', job))
            return
          }
          job.request = { ...job.request, fence: { snapshot_id: fresh.success, subject_revisions: winner === undefined ? {} : { [winner.id]: winner.revision } } }
        }
        job.requestRefused = false
        const result = await Effect.runPromise(Effect.result(actions.submitAction(job.request)), { signal: abort.signal })
        if (closed) return
        if (result._tag === 'Failure') {
          job.requestRefused = result.failure._tag === 'ActionRefused'
          const reason = arrangementRefusal(result.failure, targets(job))
          if (reason.error?.code === 'arrangement-retired' && job.request.parameters.subject === reservation) reservedRetired = true
          const refreshRequired = reason.error?.code === 'stale-fence' || reason.error?.code === 'arrangement-exists'
          fail(job, reason, refreshRequired)
          if (refreshRequired) {
            // Refetch, but NEVER automatically replay a refused edit.
            try {
              const refreshed = await refresh()
              if (refreshed._tag === 'Success' && reason.error?.code === 'arrangement-exists' &&
                sidebarWinner(inventory?.items ?? [], owner) === undefined && refusal !== undefined)
                refusal = { ...refusal, awaitingVisibility: true }
              retryReady = refreshed._tag === 'Success' && !reservedRetired
              if (retryReady) failed = prepareRetry(job)
              publish()
            } finally { job.done.resolve({ _tag: 'Refused', ...reason }) }
          }
          return
        }
        if (result.success.status === 'rejected') {
          job.requestRefused = true
          fail(job, unknown('The action result was rejected without a refusal reason.', job))
          return
        }
        if (job.request.parameters.operations.some((operation) => operation.op === 'create'))
          creation = { subject: job.request.parameters.subject, phase: 'acknowledged' }
        // Replace optimism only with a complete inventory read, not an assumed register winner.
        accept(await read(abort.signal))
        queue.shift()
        publish()
        job.done.resolve({ _tag: 'Success' })
      }
    } catch (cause) {
      if (!closed && queue[0] !== undefined) fail(queue[0], unknown(cause instanceof Error ? cause.message : String(cause), queue[0]))
      // A failed stale-fence refresh keeps retry disabled until an explicit refresh succeeds.
    } finally { running = false }
  }
  const edit = (operations: readonly SidebarOperation[]): Promise<EditOutcome> => {
    const job: EditJob = { operations: structuredClone(operations), id: `action/${crypto.randomUUID()}`, key: crypto.randomUUID(), done: Promise.withResolvers<EditOutcome>() }
    if (reservedRetired && sidebarWinner(inventory?.items ?? [], owner) === undefined)
      return Promise.resolve({ _tag: 'Refused', ...unknown('Restoring a removed Sidebar is not available yet.', job) })
    if (!running) failed = undefined // A new explicit edit is not a replay of the failed request.
    if (closed || inventory === undefined || failed !== undefined) {
      const reason = unknown(closed ? 'The arrangement editor is closed.' : inventory === undefined ? 'A complete owner inventory is required before editing.' : 'Retry or refresh the refused edit before submitting another edit.', job)
      return Promise.resolve({ _tag: 'Refused', ...reason })
    }
    queue.push(job)
    refusal = undefined
    publish() // Synchronous paint, before snapshot or HTTP work.
    void drain()
    return job.done.promise
  }
  const retryEdit = async (): Promise<EditOutcome> => {
    let job = failed
    if (job === undefined) return { _tag: 'Success' }
    if (reservedRetired && sidebarWinner(inventory?.items ?? [], owner) === undefined)
      return { _tag: 'Refused', ...(refusal ?? unknown('Restoring a removed Sidebar is not available yet.', job)) }
    if (running) return { _tag: 'Refused', ...(refusal ?? unknown('The arrangement snapshot is still refreshing.', job)) }
    if (!retryReady) {
      try {
        const fresh = await refresh()
        if (fresh._tag === 'Failure') return { _tag: 'Refused', ...arrangementRefusal(fresh.failure, targets(job)) }
        retryReady = true
        if (job.requestRefused) job = prepareRetry(job)
      } catch (cause) { return { _tag: 'Refused', ...unknown(cause instanceof Error ? cause.message : String(cause), job) } }
    }
    failed = undefined
    refusal = undefined
    const retried = { ...job, done: Promise.withResolvers<EditOutcome>() }
    queue.push(retried)
    publish()
    void drain()
    return retried.done.promise
  }
  return { accept, edit, retryEdit, close: () => {
    closed = true
    abort.abort()
    for (const job of queue.splice(0)) job.done.resolve({ _tag: 'Refused', ...unknown('The arrangement editor is closed.', job) })
  } }
}

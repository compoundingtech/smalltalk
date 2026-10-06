import type { ActionResult, Arrangement, EnvelopeOf } from '@smalltalk/st3-client'
import type { ArrangementEdit, FoldersClient, Selection } from '../client.ts'
import { selectionParts } from '../client.ts'
import { decodeDoc } from './legacy.ts'
import type { FolderDoc } from './legacy.ts'
import { importOperations } from './plan.ts'

/** Host-owned fence: stop every legacy writer and drain acknowledged/pending writes.
 * It must survive failure/restart until exclusive routing to arrangements is durable. */
export interface SourceFence { readonly source: string; readonly token: string }
export interface FrozenSnapshot { readonly doc: FolderDoc; readonly storeIndex: number }
export interface LegacySource {
  freeze(): Promise<SourceFence>
  assertFrozen(fence: SourceFence): Promise<void>
  readFrozen(fence: SourceFence): Promise<FrozenSnapshot>
}
export interface StagedMigration {
  readonly version: 1
  readonly phase: 'staged' | 'readable' | 'complete'
  readonly fence: SourceFence
  readonly source: FrozenSnapshot
  readonly request: ArrangementEdit
  readonly receipt?: ActionResult
}
export interface MigrationJournal {
  /** Serialize the entire migration across tabs/processes, not just each journal write. */
  exclusive<TValue>(run: () => Promise<TValue>): Promise<TValue>
  load(): Promise<StagedMigration | undefined>
  save(value: StagedMigration): Promise<void>
}
export interface MigrationOptions {
  readonly client: FoldersClient
  readonly source: LegacySource
  readonly journal: MigrationJournal
  readonly name: string
  readonly action: { readonly id: string; readonly idempotencyKey: string }
  /** Durable idempotent pointer cutover; never releases the fence before routing all writers. */
  readonly activate: (selection: Selection) => Promise<void>
}
export interface MigrationResult { readonly phase: 'complete'; readonly arrangement: Arrangement; readonly receipt: ActionResult }

export const migrateFolders = (options: MigrationOptions): Promise<MigrationResult> => options.journal.exclusive(async () => {
  // Capability/ownership validation happens before asking the private host to pause writers.
  const discovery = await options.client.discover()
  let staged = await options.journal.load()
  const selection = options.client.selection
  selectionParts(selection)
  if (staged === undefined) {
    const fence = await options.source.freeze()
    await options.source.assertFrozen(fence)
    const snapshot = await options.source.readFrozen(fence)
    await options.source.assertFrozen(fence)
    const doc = decodeDoc(snapshot.doc)
    const operations = importOperations(doc, options.name)
    staged = { version: 1, phase: 'staged', fence, source: { doc, storeIndex: snapshot.storeIndex }, request: {
      id: options.action.id, idempotency_key: options.action.idempotencyKey,
      fence: { snapshot_id: discovery.snapshot.id, subject_revisions: {} },
      parameters: { ...selection, operations },
    } }
    // This durable write precedes the first remote mutation. Failure leaves the source fenced.
    await options.journal.save(staged)
  }
  if (staged.request.parameters.subject !== selection.subject || staged.request.parameters.owner !== selection.owner)
    throw new TypeError('Staged migration belongs to another selected arrangement')
  if (staged.phase !== 'complete') await options.source.assertFrozen(staged.fence)
  let receipt = staged.receipt
  if (receipt === undefined) {
    // Accepted-but-lost responses replay the original receipt before fence validation.
    // No input, key or action identity is regenerated on a resume or a create race.
    receipt = (await options.client.edit({ id: staged.request.id, idempotency_key: staged.request.idempotency_key, fence: staged.request.fence }, staged.request.parameters.operations)).value
    if (receipt.status !== 'completed' || receipt.arrangement_revision === undefined ||
        receipt.action_id !== staged.request.id || !receipt.affected_ids.includes(selection.subject))
      throw new TypeError('Arrangement import did not return the matching completed revision receipt')
    staged = { ...staged, receipt }
    await options.journal.save(staged)
  }
  if (receipt.status !== 'completed' || receipt.action_id !== staged.request.id || !receipt.affected_ids.includes(selection.subject))
    throw new TypeError('Staged receipt does not match the import action and target')
  const current: EnvelopeOf<Arrangement> = await options.client.read()
  // Do not compare value/revision equality: a legitimate edit after atomic creation may
  // already be the winner. A replayed import must never overwrite that later edit.
  if (staged.phase === 'complete') return { phase: 'complete', arrangement: current.value, receipt }
  await options.source.assertFrozen(staged.fence)
  staged = { ...staged, phase: 'readable', receipt }
  await options.journal.save(staged)
  await options.activate(selection)
  // A crash after activation retries only the idempotent pointer operation, not a new import.
  await options.journal.save({ ...staged, phase: 'complete' })
  return { phase: 'complete', arrangement: current.value, receipt }
})

import { St3Client } from '@smalltalk/st3-client'
import type { ActionOf, ActionResult, Arrangement, ArrangementOperation, ArrangementPage, Capabilities, EnvelopeOf } from '@smalltalk/st3-client'
import { Schema } from 'effect'
import { Arrangement as ArrangementSchema, ArrangementPage as ArrangementPageSchema, ArrangementEditParameters as ArrangementEditSchema, Capabilities as CapabilitiesSchema } from '@smalltalk/st3-client/schema'
const isArrangement = Schema.is(Schema.toEncoded(ArrangementSchema))
const isArrangementPage = Schema.is(Schema.toEncoded(ArrangementPageSchema))
const isCapabilities = Schema.is(Schema.toEncoded(CapabilitiesSchema))
const isEdit = Schema.is(Schema.toEncoded(ArrangementEditSchema))

export type ArrangementEdit = Omit<ActionOf<'arrangement.edit'>, 'api_version' | 'type'>
export type ArrangementsGateway = Pick<St3Client, 'discover' | 'arrangementsGet' | 'arrangementsList' | 'arrangementEdit'>
export interface Selection { readonly owner: string; readonly subject: string }
export interface FoldersClient {
  readonly selection: Selection
  discover(): Promise<EnvelopeOf<Capabilities>>
  read(): Promise<EnvelopeOf<Arrangement>>
  list(cursor?: string): Promise<EnvelopeOf<ArrangementPage>>
  edit(identity: Pick<ArrangementEdit, 'id' | 'idempotency_key' | 'fence'>, operations: ArrangementOperation[]): Promise<EnvelopeOf<ActionResult>>
}
export const selectionParts = ({ owner, subject }: Selection): { person: string; uuid: string } => {
  const match = /^arrangement\/(person\/[^/\s]+)\/([0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})$/.exec(subject)
  if (!match || match[1] !== owner || match[2] === undefined) throw new TypeError('Arrangement ID must belong to the explicit person and use a lowercase UUIDv7')
  return { person: owner.slice('person/'.length), uuid: match[2] }
}
export const requireArrangements = (capabilities: Capabilities): void => {
  if (!capabilities.capabilities.some((item) => item.id === 'arrangements' && item.state === 'granted' && item.version === 1))
    throw new TypeError('Granted arrangements capability version 1 is required; older pairings may need renewal')
}

/** Online only: immutable action IDs/keys belong to the caller, not a hidden retry queue. */
export const createFoldersClient = (gateway: ArrangementsGateway, selection: Selection): FoldersClient => {
  const parts = selectionParts(selection)
  const discover = async (): Promise<EnvelopeOf<Capabilities>> => {
    const envelope = await gateway.discover()
    if (!isCapabilities(envelope.value)) throw new TypeError('Invalid generated capabilities response')
    requireArrangements(envelope.value)
    return envelope
  }
  const read = async (): Promise<EnvelopeOf<Arrangement>> => {
    await discover()
    const result = await gateway.arrangementsGet(parts.person, parts.uuid)
    if (!isArrangement(result.value) || result.value.id !== selection.subject || result.value.owner !== selection.owner)
      throw new TypeError('Arrangement response does not match the selected owner, subject and body version')
    return result
  }
  return {
    selection,
    discover,
    read,
    list: async (cursor?: string) => {
      await discover()
      const result = await gateway.arrangementsList(selection.owner, cursor === undefined ? {} : { cursor })
      if (!isArrangementPage(result.value) || result.value.items.some((item) => item.owner !== selection.owner))
        throw new TypeError('Invalid generated owner-scoped arrangements page')
      return result
    },
    edit: async (identity: Pick<ArrangementEdit, 'id' | 'idempotency_key' | 'fence'>, operations: ArrangementOperation[]) => {
      await discover()
      const parameters = { subject: selection.subject, owner: selection.owner, operations }
      if (!isEdit(parameters)) throw new TypeError('Invalid generated arrangements operations')
      return gateway.arrangementEdit({ ...identity, parameters })
    },
  }
}

// Generated arrangements are authoritative. FolderDoc is only the Tree's presentation shape.
import * as Atom from 'effect/reactivity/Atom'
import type { CollectionSocketFactory } from '@smalltalk/st3-client'
import { arrangementActions, type ArrangementActionPort } from '@st3/sdk/effect'
import { followArrangementInventory, readArrangementInventory, reservedSidebarSubject, sidebarCandidates, sidebarWinner, st3InventoryGateway, type ArrangementInventoryGateway, type InventoryFollow } from '../data/arrangements.ts'
import { emptyDoc, type FolderDoc, type Stamp } from './core.mts'
import { createArrangementEditor, refusalText, type ArrangementEditor, type ArrangementRefusal, type EditOutcome, type SidebarOperation, type StructuralIntent } from './edit.ts'
export interface FolderState {
  readonly doc: FolderDoc
  readonly phase: 'fixture' | 'connecting' | 'synced' | 'pending' | 'unavailable'
  readonly detail?: string
  readonly readOnly?: boolean
  readonly refusal?: ArrangementRefusal
  readonly sidebarCandidates?: readonly { readonly id: string; readonly label: string }[]
  readonly sidebarSubject?: string
  readonly restoreUnavailable?: boolean
  readonly edit: (operations: readonly SidebarOperation[], intent?: StructuralIntent) => Promise<EditOutcome>
  readonly retryEdit?: () => Promise<EditOutcome>
  readonly retryReady?: boolean
  readonly pendingTargets?: readonly string[]
  readonly retry?: () => void
}
export const arrangementSidebarDoc = (arrangement: Arrangement): FolderDoc => {
  const at = (revision: string): Stamp => [0, 0, revision]
  return {
    folders: Object.fromEntries(Object.entries(arrangement.body.folders).map(([id, folder]) => [id, {
      name: { value: folder.name.value, at: at(folder.name.revision) },
      position: { ...folder.position.value, at: at(folder.position.revision) },
      ...(folder.tombstone === null ? {} : { deleted: at(folder.tombstone.revision) }),
    }])),
    placements: Object.fromEntries(Object.entries(arrangement.body.placements).map(([subject, placement]) =>
      [subject, { ...placement.value, at: at(placement.revision) }])),
  }
}
export type SidebarGateway = ArrangementInventoryGateway & Pick<St3Client, 'discover'> & {
  readonly actions: ArrangementActionPort
}
/**
 * The discovered person's winning Sidebar. The complete inventory drives both the
 * projection and the edit target; serialized edits never create a second Sidebar.
 */
export const sidebarFolders = ({ gateway, socket }: {
  readonly gateway: () => SidebarGateway
  readonly socket?: CollectionSocketFactory
}): Atom.Atom<FolderState> => Atom.make((get): FolderState => {
  const client = gateway()
  let active = true
  let follow: InventoryFollow | undefined
  let editor: ArrangementEditor | undefined
  let doc = emptyDoc()
  let shown: string | undefined
  let reservedObserved = false
  let current: FolderState
  let starting = false
  const edit = (operations: readonly SidebarOperation[], intent?: StructuralIntent): Promise<EditOutcome> =>
    current.restoreUnavailable ? Promise.resolve({
      _tag: 'Refused', reason: { _tag: 'Unknown' }, detail: 'Restoring a removed Sidebar is not available yet.', targets: [],
    }) : editor?.edit(operations, intent) ?? Promise.resolve({
      _tag: 'Refused', reason: { _tag: 'Unknown' }, detail: 'A granted, complete owner inventory is required before editing.', targets: [],
    })
  const retry = () => { if (follow !== undefined) follow.refresh(); else if (!starting) void start() }
  const publish = (next: FolderState) => {
    if (!active) return
    if (next.doc === current.doc && next.phase === current.phase && next.detail === current.detail &&
      next.refusal === current.refusal && next.retryEdit === current.retryEdit && next.retryReady === current.retryReady && next.readOnly === current.readOnly &&
      (next.pendingTargets === current.pendingTargets || (next.pendingTargets?.length === current.pendingTargets?.length && next.pendingTargets?.every((target, index) => target === current.pendingTargets?.[index]))) &&
      next.sidebarSubject === current.sidebarSubject && next.restoreUnavailable === current.restoreUnavailable &&
      (next.sidebarCandidates === current.sidebarCandidates || (next.sidebarCandidates?.length === current.sidebarCandidates?.length &&
        next.sidebarCandidates?.every((candidate, index) => candidate.id === current.sidebarCandidates?.[index]?.id && candidate.label === current.sidebarCandidates?.[index]?.label)))) return
    current = next
    get.setSelf(next)
  }
  const start = async () => {
    starting = true
    publish({ ...current, phase: 'connecting', detail: 'Loading arrangements…', readOnly: true })
    try {
      const discovery = await client.discover()
      requireArrangements(discovery.value)
      const owner = /^(person\/[^/\s]+)(?:\/session\/[^/\s]+)?$/.exec(discovery.value.session_actor)?.[1]
      if (owner === undefined) throw new Error('Select an explicit person owner before loading arrangements.')
      if (!active) return
      const editable = discovery.value.capabilities.some((capability) => capability.id === 'control.arrangements' && capability.state === 'granted')
      if (editable) editor = createArrangementEditor({
        owner, actions: client.actions,
        read: (signal) => readArrangementInventory(client, owner, signal),
        onState: (state) => {
          const key = state.arrangement === undefined ? undefined : `${state.arrangement.id}\n${JSON.stringify(state.arrangement.body)}`
          if (key !== shown) { doc = state.arrangement === undefined ? emptyDoc() : arrangementSidebarDoc(state.arrangement); shown = key }
          publish({
            doc, phase: state.phase === 'refused' ? 'unavailable' : state.phase, readOnly: state.restoreUnavailable === true, edit, retry,
            ...(state.sidebarCandidates === undefined ? {} : { sidebarCandidates: state.sidebarCandidates }),
            ...(state.sidebarSubject === undefined ? {} : { sidebarSubject: state.sidebarSubject }),
            ...(state.restoreUnavailable === undefined ? {} : { restoreUnavailable: state.restoreUnavailable }),
            ...(state.pendingTargets === undefined ? {} : { pendingTargets: state.pendingTargets }),
            detail: state.refusal === undefined ? 'Arrangement folders' : refusalText(state.refusal),
            ...(state.refusal === undefined ? {} : { refusal: state.refusal }),
            ...(state.refusal !== undefined && editor !== undefined ? { retryEdit: editor.retryEdit, retryReady: state.retryReady } : {}),
          })
        },
      })
      follow = followArrangementInventory({ gateway: client, owner, ...(socket === undefined ? {} : { socket }), onEvent: (event) => {
        switch (event._tag) {
          case 'Complete': {
            if (editor !== undefined) { editor.accept(event.inventory); return }
            const winner = sidebarWinner(event.inventory.items, owner)
            const candidates = sidebarCandidates(event.inventory.items, owner)
            const reserved = reservedSidebarSubject(owner)
            reservedObserved ||= event.inventory.items.some((item) => item.id === reserved)
            const restoreUnavailable = winner === undefined && reservedObserved
            const key = winner === undefined ? undefined : `${winner.id}\n${JSON.stringify(winner.body)}`
            if (key !== shown) { doc = winner === undefined ? emptyDoc() : arrangementSidebarDoc(winner); shown = key }
            publish({
              doc, phase: 'synced', detail: 'Arrangement folders · read-only (control.arrangements is not granted)', readOnly: true, edit, retry,
              ...(candidates.length > 1 ? { sidebarCandidates: candidates.map((candidate) => ({ id: candidate.id, label: `${candidate.body.name.value} (${candidate.id})` })) } : {}),
              ...(winner === undefined ? (restoreUnavailable ? { sidebarSubject: reserved } : {}) : { sidebarSubject: winner.id }),
              ...(restoreUnavailable ? { restoreUnavailable: true } : {}),
            })
            return
          }
          case 'ReadFailed':
            publish({ ...current, phase: 'unavailable', detail: `Arrangement read failed: ${event.error.message}`, readOnly: true })
            return
          case 'Interrupted':
            publish({ ...current, phase: 'unavailable', detail: `Arrangement updates interrupted; reconnecting in ${Math.ceil(event.retryInMs / 1000)}s.`, readOnly: true })
            return
          case 'Refused':
            publish({ ...current, phase: 'unavailable', detail: `Arrangement updates refused: ${event.error.message}`, readOnly: true })
        }
      } })
    } catch (error) {
      publish({ ...current, phase: 'unavailable', detail: error instanceof Error ? error.message : String(error), readOnly: true })
    } finally { starting = false }
  }
  get.addFinalizer(() => { active = false; follow?.close(); editor?.close() })
  current = { doc, phase: 'connecting', readOnly: true, edit, retry }
  void Promise.resolve().then(start)
  return current
})
/** Same-origin paired client API, scoped to the discovered person. */
export const folders = sidebarFolders({
  gateway: () => {
    const options = { baseUrl: globalThis.location.origin, fetchImpl: globalThis.fetch.bind(globalThis) }
    return { ...st3InventoryGateway(options), actions: arrangementActions(new St3Client(options)) }
  },
})

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

// Sidebar adapter is read-only. Generated arrangements stay authoritative; this document is
// only the existing Tree's presentation shape, never a legacy replica or an edit input.
import * as Atom from 'effect/reactivity/Atom'
import type { CollectionSocketFactory } from '@smalltalk/st3-client'
import { followArrangementInventory, sidebarWinner, st3InventoryGateway, type ArrangementInventoryGateway, type InventoryFollow } from '../data/arrangements.ts'
import { emptyDoc, type FolderDoc, type FolderOp, type Stamp } from './core.mts'
export interface FolderState {
  readonly doc: FolderDoc
  readonly phase: 'fixture' | 'connecting' | 'synced' | 'pending' | 'unavailable'
  readonly detail?: string
  readonly readOnly?: boolean
  readonly edit: (operations: readonly FolderOp[]) => void
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
export type SidebarGateway = ArrangementInventoryGateway & Pick<St3Client, 'discover'>
/**
 * The discovered person's live Sidebar: the owner's lowest-UUIDv7 arrangement, taken from a
 * complete owner inventory that every owner-wide arrangements frame re-reads.
 */
export const sidebarFolders = ({ gateway, socket }: {
  readonly gateway: () => SidebarGateway
  /** Test seam for the collections WebSocket. */
  readonly socket?: CollectionSocketFactory
}): Atom.Atom<FolderState> => Atom.make((get): FolderState => {
  const client = gateway()
  let active = true
  let follow: InventoryFollow | undefined
  let doc = emptyDoc()
  // Winner ID plus body text: an unchanged reread keeps the Tree's document identity.
  let shown: string | undefined
  let current: Pick<FolderState, 'phase' | 'detail'> = { phase: 'connecting' }
  const edit = () => { throw new Error('Arrangement sidebar is read-only; edits are unavailable.') }
  let starting = false
  const retry = () => { if (follow !== undefined) follow.refresh(); else if (!starting) void start() }
  const publish = (phase: FolderState['phase'], detail: string, next = doc) => {
    if (!active || (next === doc && phase === current.phase && detail === current.detail)) return
    doc = next
    current = { phase, detail }
    get.setSelf({ doc, phase, detail, readOnly: true, edit, retry })
  }
  const start = async () => {
    starting = true
    publish('connecting', 'Loading arrangements…')
    try {
      const discovery = await client.discover()
      requireArrangements(discovery.value)
      // A person session actor is person/<id>/session/<id>; machine/agent actors cannot choose a person.
      const owner = /^(person\/[^/\s]+)(?:\/session\/[^/\s]+)?$/.exec(discovery.value.session_actor)?.[1]
      if (owner === undefined) throw new Error('Select an explicit person owner before loading arrangements.')
      if (!active) return
      follow = followArrangementInventory({ gateway: client, owner, ...(socket === undefined ? {} : { socket }), onEvent: (event) => {
        switch (event._tag) {
          case 'Complete': {
            const winner = sidebarWinner(event.inventory.items)
            if (winner === undefined) {
              shown = undefined
              publish('unavailable', 'No arrangement exists for this person yet. Folders appear once a native client creates one.', emptyDoc())
              return
            }
            const key = `${winner.id}\n${JSON.stringify(winner.body)}`
            const next = key === shown ? doc : arrangementSidebarDoc(winner)
            shown = key
            publish('synced', 'Arrangement folders · read-only', next)
            return
          }
          case 'ReadFailed':
            publish('unavailable', `Arrangement read failed: ${event.error.message}`)
            return
          case 'Interrupted':
            publish('unavailable', `Arrangement updates interrupted; reconnecting in ${Math.ceil(event.retryInMs / 1000)}s.`)
            return
          case 'Refused':
            publish('unavailable', `Arrangement updates refused: ${event.error.message}`)
        }
      } })
    } catch (error) {
      publish('unavailable', error instanceof Error ? error.message : String(error))
    } finally {
      starting = false
    }
  }
  get.addFinalizer(() => { active = false; follow?.close() })
  void Promise.resolve().then(start)
  return { doc, phase: 'connecting', readOnly: true, edit, retry }
})
/** Same-origin paired client API, scoped to the discovered person. */
export const folders = sidebarFolders({
  gateway: () => st3InventoryGateway({ baseUrl: globalThis.location.origin, fetchImpl: globalThis.fetch.bind(globalThis) }),
})

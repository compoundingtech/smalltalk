import { St3Client, type CollectionStreamOptions } from '@smalltalk/st3-client'
import { applyWindow } from '@smalltalk/st3-client'
import type { ActionOf, ActionResult, Arrangement, ArrangementOperation, ArrangementPage, Capabilities, CollectionWindow, CollectionStream, EnvelopeOf } from '@smalltalk/st3-client'
import { Schema } from 'effect'
import { Arrangement as ArrangementSchema, ArrangementPage as ArrangementPageSchema, ArrangementEditParameters as ArrangementEditSchema, Capabilities as CapabilitiesSchema, CollectionFrame as CollectionFrameSchema } from '@smalltalk/st3-client/schema'
const isArrangement = Schema.is(Schema.toEncoded(ArrangementSchema))
const isArrangementPage = Schema.is(Schema.toEncoded(ArrangementPageSchema))
const isCapabilities = Schema.is(Schema.toEncoded(CapabilitiesSchema))
const isFrame = Schema.is(Schema.toEncoded(CollectionFrameSchema))
const isEdit = Schema.is(Schema.toEncoded(ArrangementEditSchema))

export type ArrangementEdit = Omit<ActionOf<'arrangement.edit'>, 'api_version' | 'type'>
export type ArrangementsGateway = Pick<St3Client, 'discover' | 'arrangementsGet' | 'arrangementsList' | 'arrangementEdit' | 'collectionStream'>
export interface Selection { readonly owner: string; readonly subject: string }
export interface FoldersClient {
  readonly selection: Selection
  discover(): Promise<EnvelopeOf<Capabilities>>
  read(): Promise<EnvelopeOf<Arrangement>>
  list(cursor?: string): Promise<EnvelopeOf<ArrangementPage>>
  edit(identity: Pick<ArrangementEdit, 'id' | 'idempotency_key' | 'fence'>, operations: ArrangementOperation[]): Promise<EnvelopeOf<ActionResult>>
  watch(onValue: (value: Arrangement | undefined) => void, options?: Pick<CollectionStreamOptions, 'onEnd' | 'socket'>): Promise<CollectionStream>
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
    /** Selected-subject windows cannot lose the sidebar to an owner's 100-row prefix. */
    watch: async (onValue: (value: Arrangement | undefined) => void, options: Pick<CollectionStreamOptions, 'onEnd' | 'socket'> = {}) => {
      await discover()
      let window: CollectionWindow | undefined
      let connection: CollectionStream | undefined
      let invalid = false
      const stream = await gateway.collectionStream({ ...options, onFrame: (frame) => {
        if (!isFrame(frame)) {
          if (invalid) return
          invalid = true
          connection?.close()
          options.onEnd?.(new TypeError('Invalid generated arrangement collection frame'))
          return
        }
        if ((frame.kind !== 'snapshot' && frame.kind !== 'changes') || frame.id !== 'folders' || frame.collection !== 'arrangements') return
        window = applyWindow(window, frame)
        if (window === undefined) return
        const value = window.items.find((item): item is Arrangement => item.kind === 'arrangement' && item.id === selection.subject)
        onValue(value)
      } })
      connection = stream
      if (invalid) { stream.close(); return stream }
      stream.subscribeArrangements('folders', selection.owner, 100, selection.subject)
      return stream
    },
  }
}


// Sidebar adapter is read-only. Generated arrangements stay authoritative; this document is
// only the existing Tree's presentation shape, never a legacy replica or an edit input.
import * as Atom from 'effect/reactivity/Atom'
import { emptyDoc, type FolderDoc, type FolderOp, type Stamp } from './core.mts'
export interface FolderState {
  readonly doc: FolderDoc
  readonly phase: 'fixture' | 'connecting' | 'synced' | 'pending' | 'unavailable'
  readonly detail?: string
  readonly readOnly?: boolean
  readonly edit: (operations: readonly FolderOp[]) => void
  readonly retry?: () => void
  readonly selected?: string
  readonly choices?: readonly { readonly id: string; readonly name: string }[]
  readonly select?: (subject: string) => void
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
/** Same-origin paired client API, scoped to the discovered person and an explicit arrangement. */
export const folders = Atom.make((get): FolderState => {
  const gateway = new St3Client({ baseUrl: globalThis.location.origin, fetchImpl: globalThis.fetch.bind(globalThis) })
  let active = true
  let generation = 0
  let stream: CollectionStream | undefined
  let selected: string | undefined
  let choices: FolderState['choices'] = []
  let doc = emptyDoc()
  const edit = () => { throw new Error('Arrangement sidebar is read-only; edits are unavailable.') }
  const publish = (phase: FolderState['phase'], detail?: string) => {
    if (active) get.setSelf({ doc, phase, detail, readOnly: true, edit, retry: () => { void load() }, choices, selected, select })
  }
  const select = (subject: string) => {
    if (!choices?.some(choice => choice.id === subject)) return
    selected = subject
    void load()
  }
  const load = async () => {
    const current = ++generation
    stream?.close()
    stream = undefined
    publish('connecting', 'Loading arrangements…')
    try {
      const discovery = await gateway.discover()
      requireArrangements(discovery.value)
      // A person session actor is person/<id>/session/<id>; machine/agent actors cannot choose a person.
      const actor = /^(person\/[^/\s]+)(?:\/session\/[^/\s]+)?$/.exec(discovery.value.session_actor)
      const owner = actor?.[1]
      if (owner === undefined) throw new Error('Select an explicit person owner before loading arrangements.')
      const arrangements: Arrangement[] = []
      let cursor: string | undefined
      const visited = new Set<string>()
      do {
        const page = await gateway.arrangementsList(owner, cursor === undefined ? {} : { cursor })
        if (!isArrangementPage(page.value) || page.value.items.some(item => item.owner !== owner))
          throw new Error('Invalid owner-scoped arrangements page')
        arrangements.push(...page.value.items)
        cursor = page.value.page.has_more ? page.value.page.next_cursor ?? undefined : undefined
        if (page.value.page.has_more && cursor === undefined) throw new Error('Arrangement page omitted its continuation cursor')
        if (cursor !== undefined && visited.has(cursor)) throw new Error('Arrangement pagination repeated its cursor')
        if (cursor !== undefined) visited.add(cursor)
      } while (cursor !== undefined)
      if (!active || current !== generation) return
      choices = arrangements.map(item => ({ id: item.id, name: item.body.name.value }))
      if (selected === undefined && arrangements.length === 1) selected = arrangements[0]!.id
      if (selected === undefined) {
        publish('unavailable', arrangements.length === 0 ? 'No arrangements exist for this person.' : 'Select an arrangement to display folders.')
        return
      }
      const client = createFoldersClient(gateway, { owner, subject: selected })
      const result = await client.read()
      if (!active || current !== generation) return
      doc = arrangementSidebarDoc(result.value)
      publish('synced', 'Arrangement folders · read-only')
      const connection = await client.watch(value => {
        if (!active || current !== generation) return
        if (value === undefined) { publish('unavailable', 'Selected arrangement is unavailable.'); return }
        doc = arrangementSidebarDoc(value)
        publish('synced', 'Arrangement folders · read-only')
      }, { onEnd: error => {
        if (active && current === generation) publish('unavailable', `Arrangement updates unavailable: ${String(error ?? 'connection closed')}`)
      } })
      if (!active || current !== generation) connection.close()
      else stream = connection
    } catch (error) {
      if (active && current === generation) publish('unavailable', error instanceof Error ? error.message : String(error))
    }
  }
  get.addFinalizer(() => { active = false; generation++; stream?.close() })
  void Promise.resolve().then(load)
  return { doc, phase: 'connecting', readOnly: true, edit, retry: () => { void load() } }
})

import type { St3Client, CollectionStreamOptions } from '@smalltalk/st3-client'
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

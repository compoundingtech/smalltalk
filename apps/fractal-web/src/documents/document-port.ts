import { ClientError, plainError, type St3Client } from '@smalltalk/st3-client'

/**
 * One read of an st document by name (the newest version) or by its immutable `name@hash`
 * reference: its text, or why it cannot be shown.
 */
export type DocumentRead =
  | { readonly _tag: 'Text'; readonly reference: string; readonly text: string }
  | { readonly _tag: 'Binary'; readonly reference: string; readonly size: number }
  | { readonly _tag: 'Unavailable'; readonly reason: 'not-found' | 'ungranted' | 'failed'; readonly detail: string }

export interface DocumentPort {
  readonly read: (name: string) => Promise<DocumentRead>
}

/** A document name or reference as st writes one: `doc/` and a path, optionally `@` its hash. */
export const DOCUMENT_REF = /^doc\/[A-Za-z0-9._~\-/]+(?:@[0-9a-f]{6,64})?$/

/** `doc/name` and the short version from a `doc/name@hash` reference. */
export const documentVersion = (reference: string): { readonly name: string; readonly version?: string } => {
  const at = reference.lastIndexOf('@')
  return at < 0 ? { name: reference } : { name: reference.slice(0, at), version: reference.slice(at + 1, at + 13) }
}

const utf8 = new TextDecoder('utf-8', { fatal: true })

/** Reads through `GET /v1/client/documents/content`, the route stui and the phone read. */
export const gatewayDocuments = (client: St3Client): DocumentPort => ({
  read: async (name) => {
    try {
      const { reference, bytes } = (await client.documentGet(name)).value
      const raw = Uint8Array.from(bytes)
      try {
        return { _tag: 'Text', reference, text: utf8.decode(raw) }
      } catch {
        return { _tag: 'Binary', reference, size: raw.length }
      }
    } catch (error) {
      if (error instanceof ClientError && error.response.code === 'not-found') {
        return { _tag: 'Unavailable', reason: 'not-found', detail: `There is no document ${name}.` }
      }
      if (error instanceof ClientError && (error.response.code === 'forbidden' || error.status === 403)) {
        return { _tag: 'Unavailable', reason: 'ungranted', detail: 'Reading documents has not been granted to this device.' }
      }
      return { _tag: 'Unavailable', reason: 'failed', detail: `The document could not be read: ${plainError(error)}.` }
    }
  },
})

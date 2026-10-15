import type { ConversationPortResult, Grants } from '../data/source.ts'

export type ComposerRefusal = { readonly reason: string }
  & Pick<Extract<ConversationPortResult<never>, { _tag: 'Refused' }>, 'error' | 'status'>
export type ComposerAvailability =
  | { readonly _tag: 'Ready' }
  | { readonly _tag: 'Offline'; readonly detail: string }
  | { readonly _tag: 'MissingGrant'; readonly detail: string }
  | { readonly _tag: 'Revoked'; readonly detail: string; readonly code?: string }
  | { readonly _tag: 'Unavailable'; readonly detail: string }

/** Native auth refusal is not a missing capability. No state transition dispatches mail. */
export const composerAvailability = ({ online, readable, granted, refusal }: {
  readonly online: boolean
  readonly readable: boolean
  readonly granted: Grants['messageSend']
  readonly refusal?: ComposerRefusal
}): ComposerAvailability => {
  if (refusal?.status === 401 || refusal?.status === 403 || refusal?.error?.code === 'issuer-required')
    return { _tag: 'Revoked', detail: 'This device credential was revoked. Reconnect or pair this device again to send.',
      ...(refusal.error === undefined ? {} : { code: refusal.error.code }) }
  if (!online) return { _tag: 'Offline', detail: 'Offline — reconnect to send. Your draft stays editable.' }
  if (granted !== 'granted') return { _tag: 'MissingGrant', detail: 'Message send is not granted for this device.' }
  if (!readable) return { _tag: 'Unavailable', detail: 'Messages can be sent once this conversation loads.' }
  return { _tag: 'Ready' }
}

import { ClientError, plainError, type St3Client } from '@smalltalk/st3-client'
import type * as Native from '@smalltalk/st3-client/schema'

/** One answer to a native prompt: sent, or refused in words a person reads. */
export type PromptAnswerResult =
  | { readonly _tag: 'Sent' }
  | { readonly _tag: 'Refused'; readonly detail: string }

/** A native harness prompt's alert, as much of it as an answer names. */
export type PromptAlert = Pick<Native.Attention, 'id' | 'revision' | 'source_id'> & {
  readonly target_id: string
  readonly episode: string
}

/**
 * Answers native harness prompts (`prompt.respond`): `allow`/`deny`, `approve`, a question's
 * choices, or a screen prompt's own answer, as the alert offers them.
 */
export interface PromptAnswerPort {
  readonly respond: (request: {
    readonly alert: PromptAlert
    /** What is sent as the answer: the answer itself, or a question's choices as JSON. */
    readonly answer: string
  }) => Promise<PromptAnswerResult>
}

/**
 * One fresh fence and one submission, fenced on the alert's own revision. A prompt st says is
 * gone or answered only in its terminal is never sent again, and neither is any other refusal: the
 * person sees why and answers again if it still waits.
 */
export const gatewayPromptAnswers = (client: St3Client): PromptAnswerPort => ({
  respond: async ({ alert, answer }) => {
    try {
      const observed = await client.capabilities()
      // Only the grant and the snapshot are read: the rest of the document is not this port's.
      const granted = ((observed.value as { capabilities?: ReadonlyArray<{ id?: unknown; state?: unknown }> }).capabilities ?? [])
        .some((capability) => capability.id === 'prompt.respond' && capability.state === 'granted')
      if (!granted) {
        return {
          _tag: 'Refused',
          detail: 'This device is not granted answering prompts. Answer it in the seat\'s terminal.',
        }
      }
      const snapshot = (observed.snapshot as { id?: unknown } | undefined)?.id
      if (typeof snapshot !== 'string') {
        return { _tag: 'Refused', detail: 'st did not say what it last saw. Try again in a moment.' }
      }
      const id = `action/${crypto.randomUUID()}`
      const response = await client.promptRespond({
        id,
        idempotency_key: id,
        fence: { snapshot_id: snapshot, subject_revisions: { [alert.id]: alert.revision } },
        parameters: { target_id: alert.target_id, episode: alert.episode, answer },
      })
      const status = (response.value as { status?: unknown }).status
      if (status === 'accepted' || status === 'completed') return { _tag: 'Sent' }
      return status === 'rejected'
        ? { _tag: 'Refused', detail: 'st did not take this answer. Answer it in the seat\'s terminal.' }
        : { _tag: 'Refused', detail: 'st did not say whether it took this answer; check the alert before answering again.' }
    } catch (error) {
      return {
        _tag: 'Refused',
        detail:
          error instanceof ClientError
            ? `${plainError(error)}.`
            : 'The answer could not be confirmed; check the alert before answering again. It was not sent again.',
      }
    }
  },
})

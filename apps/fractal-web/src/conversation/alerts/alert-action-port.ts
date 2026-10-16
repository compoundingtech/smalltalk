import { ClientError, plainError, type St3Client } from '@smalltalk/st3-client'
import type * as Native from '@smalltalk/st3-client/schema'

/** One answer to an alert: sent, or refused in words a person reads. */
export type AlertActionResult =
  | { readonly _tag: 'Sent' }
  | { readonly _tag: 'Refused'; readonly detail: string }

/** The alert as the person read it: what an answer names and is fenced on. */
export type AlertAnswered = Pick<Native.Attention, 'id' | 'revision' | 'source_id' | 'episode'>

/**
 * An answer to an ask or a review gate, as stui and the phone send it:
 * `work.done` completes the waiting person step with a summary and, for a structured request,
 * the named answer; `review.*` decides a human gate, with the person's words as its reason.
 */
export type AlertAction =
  | {
      readonly type: 'work.done'
      readonly summary: string
      readonly answer?: { readonly id?: string; readonly text?: string }
    }
  | {
      readonly type: 'review.approve' | 'review.reject' | 'review.request-changes'
      readonly reason?: string
    }

export interface AlertActionPort {
  readonly act: (request: { readonly alert: AlertAnswered; readonly action: AlertAction }) => Promise<AlertActionResult>
}

const refused = (detail: string): AlertActionResult => ({ _tag: 'Refused', detail })

const REVIEW = {
  'review.approve': (client: St3Client) => client.reviewApprove.bind(client),
  'review.reject': (client: St3Client) => client.reviewReject.bind(client),
  'review.request-changes': (client: St3Client) => client.reviewRequestChanges.bind(client),
} as const

/**
 * Reads the alert again, then submits once, fenced on the revision the person read. An alert st
 * no longer holds open, or that no longer offers this answer, is not answered; neither is one
 * that changed since it was read. A failure is never retried: the person sees why.
 */
export const gatewayAlertActions = (client: St3Client): AlertActionPort => ({
  act: async ({ alert, action }) => {
    try {
      let current
      try {
        current = await client.attentionGet(alert.id)
      } catch (error) {
        if (error instanceof ClientError && error.response.code === 'not-found') {
          return refused('This was already answered or closed.')
        }
        throw error
      }
      const item = current.value as { kind?: unknown; state?: unknown; revision?: unknown; actions?: readonly unknown[] }
      if (item.kind !== 'attention' || item.state !== 'open') return refused('This was already answered or closed.')
      if (!(item.actions ?? []).includes(action.type)) return refused('st no longer offers this answer here.')
      if (item.revision !== alert.revision) return refused('This changed since you read it. Read it again before answering.')
      const id = `action/${crypto.randomUUID()}`
      const fence = { snapshot_id: current.snapshot.id, subject_revisions: { [alert.id]: alert.revision } }
      const response =
        action.type === 'work.done'
          ? await client.workDone({
              id,
              idempotency_key: id,
              fence,
              parameters: {
                target_id: alert.source_id,
                episode: alert.episode || alert.revision,
                summary: action.summary,
                ...(action.answer === undefined ? {} : { answer: action.answer }),
              },
            })
          : await REVIEW[action.type](client)({
              id,
              idempotency_key: id,
              fence,
              parameters: { target_id: alert.source_id, ...(action.reason ? { reason: action.reason } : {}) },
            })
      return (response.value as { status?: unknown }).status === 'rejected'
        ? refused('st did not take this answer.')
        : { _tag: 'Sent' }
    } catch (error) {
      return refused(
        error instanceof ClientError
          ? `${plainError(error)}.`
          : 'The answer could not be confirmed; check the alert before answering again. It was not sent again.',
      )
    }
  },
})

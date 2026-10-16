import { ClientError, plainError, type St3Client } from '@smalltalk/st3-client'
import type * as Native from '@smalltalk/st3-client/schema'

/** One answer to an alert: sent, or refused in words a person reads. */
export type AlertActionResult =
  | { readonly _tag: 'Sent' }
  | { readonly _tag: 'Refused'; readonly detail: string }

/** The alert as the person read it: what an answer names and is fenced on. */
export type AlertAnswered = Pick<Native.Attention, 'id' | 'revision' | 'source_id' | 'episode'>

/**
 * An answer to an alert, as stui and the phone send it:
 * `work.done` completes the waiting person step with a summary and, for a structured request,
 * the named answer; `review.*` decides a human gate, with the person's words as its reason;
 * `launch.*` approves the planner's current preview, sends it back with the person's words,
 * or cancels the launch.
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
  | { readonly type: 'launch.approve' | 'launch.cancel' }
  | { readonly type: 'launch.revise'; readonly feedback: string }

export interface AlertActionPort {
  readonly act: (request: { readonly alert: AlertAnswered; readonly action: AlertAction }) => Promise<AlertActionResult>
}

const refused = (detail: string): AlertActionResult => ({ _tag: 'Refused', detail })

const REVIEW = {
  'review.approve': (client: St3Client) => client.reviewApprove.bind(client),
  'review.reject': (client: St3Client) => client.reviewReject.bind(client),
  'review.request-changes': (client: St3Client) => client.reviewRequestChanges.bind(client),
} as const

type Read = { readonly kind?: unknown; readonly id?: unknown; readonly revision?: unknown }
type AttentionRead = Read & {
  readonly state?: unknown
  readonly actions?: readonly unknown[]
  readonly source_id?: unknown
  readonly launch_id?: unknown
  readonly variant_id?: unknown
  readonly preview_token?: unknown
  readonly episode?: unknown
}
type VariantRead = Read & { readonly ordinal?: unknown; readonly preview_token?: unknown }

/** The launch an approval is about: named on the alert, else its planning session's own id. */
const launchOf = (item: AttentionRead): string =>
  typeof item.launch_id === 'string' ? item.launch_id : String(item.source_id).replace(/^planning-session\//, '')

/**
 * The variant the alert showed, by the id and preview token it names, or why there is none to
 * approve. The alert is read again at the same revision the person read, so these are what they
 * read: a newer variant, or the same one previewed again, is never approved in its place.
 */
const shownVariant = (items: readonly unknown[], item: AttentionRead): VariantRead | 'unnamed' | 'changed' => {
  // An open launch alert's episode is its preview's hash, which is the preview token.
  const token = typeof item.preview_token === 'string' ? item.preview_token : typeof item.episode === 'string' ? item.episode : undefined
  if (typeof item.variant_id !== 'string' || token === undefined) return 'unnamed'
  const variant = (items as readonly VariantRead[]).find((each) => each.kind === 'launch-variant' && each.id === item.variant_id)
  return variant !== undefined && variant.preview_token === token ? variant : 'changed'
}

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
      const item = current.value as AttentionRead
      if (item.kind !== 'attention' || item.state !== 'open') return refused('This was already answered or closed.')
      // A launch is sent back to its planner while st offers to approve it.
      const offers = action.type === 'launch.revise' ? 'launch.approve' : action.type
      if (!(item.actions ?? []).includes(offers)) return refused('st no longer offers this answer here.')
      if (item.revision !== alert.revision) return refused('This changed since you read it. Read it again before answering.')
      const id = `action/${crypto.randomUUID()}`
      const seen = { [alert.id]: alert.revision }
      const response = await (async () => {
        switch (action.type) {
          case 'work.done':
            return client.workDone({
              id,
              idempotency_key: id,
              fence: { snapshot_id: current.snapshot.id, subject_revisions: seen },
              parameters: {
                target_id: alert.source_id,
                episode: alert.episode || alert.revision,
                summary: action.summary,
                ...(action.answer === undefined ? {} : { answer: action.answer }),
              },
            })
          case 'review.approve':
          case 'review.reject':
          case 'review.request-changes':
            return REVIEW[action.type](client)({
              id,
              idempotency_key: id,
              fence: { snapshot_id: current.snapshot.id, subject_revisions: seen },
              parameters: { target_id: alert.source_id, ...(action.reason ? { reason: action.reason } : {}) },
            })
          case 'launch.approve':
          case 'launch.cancel':
          case 'launch.revise': {
            // A launch is fenced on the launch as st holds it now, as stui fences it.
            const launchId = launchOf(item)
            const launch = await client.launchesGet(launchId)
            const read = launch.value as Read
            if (read.kind !== 'launch' || typeof read.revision !== 'string') return 'gone' as const
            const fence = { snapshot_id: launch.snapshot.id, subject_revisions: { ...seen, [launchId]: read.revision } }
            if (action.type === 'launch.cancel') {
              return client.launchCancel({ id, idempotency_key: id, fence, parameters: { target_id: launchId } })
            }
            if (action.type === 'launch.revise') {
              return client.launchRevise({
                id,
                idempotency_key: id,
                fence: { snapshot_id: launch.snapshot.id, subject_revisions: { [launchId]: read.revision } },
                parameters: { launch_id: launchId, feedback: action.feedback },
              })
            }
            const variants = await client.launchVariantsList(launchId, { limit: 50 })
            const variant = shownVariant((variants.value as { items?: readonly unknown[] }).items ?? [], item)
            if (variant === 'unnamed') return 'unnamed' as const
            if (variant === 'changed' || typeof variant.id !== 'string' || typeof variant.revision !== 'string') return 'changed' as const
            return client.launchApprove({
              id,
              idempotency_key: id,
              fence: {
                ...fence,
                subject_revisions: { ...fence.subject_revisions, [variant.id]: variant.revision },
                preview_token: variant.preview_token as string,
              },
              parameters: { launch_id: launchId, variant_id: variant.id },
            } as Parameters<St3Client['launchApprove']>[0])
          }
        }
      })()
      if (response === 'gone') return refused('This launch is gone.')
      if (response === 'unnamed') return refused('This alert does not say which preview it shows, so it cannot be approved here. Review it in st.')
      if (response === 'changed') return refused('The preview this alert shows has changed. Read it again before approving.')
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

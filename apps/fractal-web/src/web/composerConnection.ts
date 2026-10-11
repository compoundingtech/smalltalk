import type { FeedSyncObservation } from '../data/feedSync.ts'
import type { NetworkReachability } from '../data/source.ts'

/** App-owned facts and action for the composer's connection-notice slot. */
export interface ComposerConnectionNotice {
  readonly tone: 'offline' | 'reconnecting'
  readonly text: string
  readonly action?: { readonly label: string; readonly onPress: () => void }
}
export interface ComposerConnectionProps {
  readonly connectionNotice?: ComposerConnectionNotice
}
const noConnectionNotice: ComposerConnectionProps = {}

/** Browser offline is immediate; uncertain transport loss uses the existing two-second grace.
 * A browser signal cannot prove that a particular host is offline. */
export const composerConnectionProps = ({ network, observation, now, gateway, reconnect }: {
  readonly network: NetworkReachability | undefined
  readonly observation: FeedSyncObservation | undefined
  readonly now: number
  readonly gateway?: string
  readonly reconnect?: () => void
}): ComposerConnectionProps => {
  const offline = network?._tag === 'Offline'
  if (!offline && (observation?.status._tag !== 'Stale' || observation.status.reason._tag !== 'Reconnecting' || now - observation.observedAt < 2000))
    return noConnectionNotice
  return {
    connectionNotice: {
      tone: offline ? 'offline' : 'reconnecting',
      text: offline ? 'You are offline' : gateway === undefined ? 'Connection lost · reconnecting' : `${gateway} connection lost · reconnecting`,
      ...(reconnect === undefined ? {} : { action: { label: 'Reconnect now', onPress: reconnect } }),
    },
  }
}

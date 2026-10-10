import type { Attention } from '@smalltalk/st3-client/schema'
import type { ReactNode } from 'react'
import { useAgentAttentionSelection } from '../data/react.tsx'
import type { Feed } from '../data/source.ts'
import { WfIcon } from '../icons/WfIcon.tsx'

interface AttentionCounts {
  readonly inbox: number
  readonly decisions: number
  readonly attentionStale: boolean
}
const attentionCounts = (feed: Feed<readonly Attention[]>): AttentionCounts => {
  let inbox = 0
  let decisions = 0
  if (feed._tag === 'Observed')
    for (const card of feed.value)
      if (card.attention_kind === 'unread-message') inbox++
      else decisions++
  return { inbox, decisions, attentionStale: feed._tag === 'Observed' && feed.freshness === 'stale' }
}
// oxlint-disable-next-line overeng/named-args -- Atom.withEquality fixed comparator ABI.
const sameCounts = (left: AttentionCounts, right: AttentionCounts): boolean =>
  left.inbox === right.inbox && left.decisions === right.decisions && left.attentionStale === right.attentionStale
export const useAgentAttentionCounts = (agentRef: string): AttentionCounts =>
  useAgentAttentionSelection({ ref: agentRef, select: attentionCounts, equal: sameCounts })

/** Attention is a public gateway feature, independent of private ledger attribution. */
export const AgentAttentionSignals = ({ agentRef }: { readonly agentRef: string }): ReactNode => {
  const { inbox, decisions, attentionStale } = useAgentAttentionCounts(agentRef)
  return <>
    {inbox === 0 ? null : <span aria-label={`${inbox} inbox items${attentionStale ? ", stale" : ""}`}><WfIcon name="inbox" />{inbox}</span>}
    {decisions === 0 ? null : <span aria-label={`${decisions} decisions or other attention${attentionStale ? ", stale" : ""}`}><WfIcon name="attention" />{decisions}</span>}
  </>
}

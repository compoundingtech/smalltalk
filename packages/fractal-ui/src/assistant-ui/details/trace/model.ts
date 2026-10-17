/** Agent-trace provider payload types; runtime provider decoding stays outside the UI. */
import type { ActivitySegment } from './types.ts'

/** Supported lookback window encoded as a compact duration literal. */
export type AgentTraceHorizon = '1h' | '1d' | '7d'
/** A session's structural role in the agent delegation graph. */
export type AgentSessionRole = 'main' | 'subagent' | 'advisor'

/** Exact window aggregates; bounded requests, bins and events are drawing marks only. */
export interface AgentTraceSessionFacts {
  readonly requests: number
  readonly turns: number
  readonly usd: number
  readonly inputTokens: number
  readonly cacheReadTokens: number
  readonly cacheWriteTokens: number
  readonly outputTokens: number
  readonly toolCalls: number
  readonly compactions: number
  readonly prompts: number
  readonly activeMs: number
  readonly lastRequestAt: number
  readonly lastExitAt: number | null
  readonly status: 'active' | 'inactive' | 'exited'
}

/** Normalized session identity, hierarchy, timing, and optionally precomputed activity. */
export interface AgentTraceSession {
  readonly id: string
  readonly parentId: string | null
  readonly rootId: string
  readonly depth: number
  readonly role: AgentSessionRole
  readonly model: string
  readonly actorId: string
  readonly project: string | null // `repo@branch` worktree key
  readonly title: string | null
  readonly name: string | null // child display name; root: null
  readonly agentType: string | null // child agent type; root persona
  readonly task: string | null // neutralized short summary, at most 120 characters; root: null
  readonly modelRole: string | null // session_init.modelRole
  readonly readOnly: boolean // session_init.readOnly; false for roots
  readonly contextCapacity: number | null // token capacity of the session's initial model
  readonly startedAt: number // epoch ms, first observed request
  readonly endedAt: number | null // epoch ms of session_exit event, null if none observed
  readonly activity?: ReadonlyArray<ActivitySegment>
  readonly activeMs?: number
  readonly activitySampled?: boolean
  readonly facts?: AgentTraceSessionFacts
}

/** One retained ledger request with epoch-millisecond timing, token counts, and USD spend. */
export interface AgentTraceRequest {
  readonly sessionId: string
  readonly ts: number // epoch ms
  readonly durationMs: number | null
  readonly kind: 'turn' | 'auxiliary'
  readonly stopReason: string // 'stop' | 'toolUse' | 'error' | ...
  readonly provider: string // requests.provider; '' for fixtures captured before it was recorded
  readonly model: string
  /** Context window from the ledger's model catalog for (provider, model); null when unknown. */
  readonly contextCapacity: number | null
  readonly inputTokens: number
  readonly cacheReadTokens: number
  readonly cacheWriteTokens: number
  readonly outputTokens: number
  readonly contextTokens: number | null // context_prompt_tokens (+ nonmessage) snapshot if present
  readonly compactionEpoch: number
  readonly usd: number // cost_total_usd, API-equivalent
  readonly usdInput: number
  readonly usdCacheRead: number
  readonly usdCacheWrite: number
  readonly usdOutput: number
  readonly requests: 1
  readonly toolCalls: number
  readonly tools: ReadonlyArray<string>
}

/**
 * Context fill range for one compaction segment of a bin: requests between compaction boundaries,
 * with fractions computed per request (context tokens / context window) before taking min/max.
 */
export interface AgentTraceContextSegment {
  readonly from: number // epoch ms of the segment's first request
  readonly to: number // epoch ms of the segment's last request
  readonly compactionEpoch: number
  readonly minFraction: number | null // null when no request in the segment has a known capacity
  readonly maxFraction: number | null
  readonly measured: number // requests with a known context size and capacity
  readonly unknown: number // requests whose capacity or context size is unknown
}

/** Aggregate evidence for request rows folded into an epoch-millisecond time interval. */
export interface AgentTraceBin {
  readonly sessionId: string
  readonly from: number
  readonly to: number
  readonly requests: number
  readonly firstTs?: number
  readonly lastTs?: number
  readonly agentTurns: number
  readonly usd: number
  readonly usdInput: number
  readonly usdCacheRead: number
  readonly usdCacheWrite: number
  readonly usdOutput: number
  readonly inputTokens: number
  readonly cacheReadTokens: number
  readonly cacheWriteTokens: number
  readonly outputTokens: number
  readonly toolCalls: number
  readonly tools: ReadonlyArray<string>
  readonly byModel: Readonly<Record<string, { readonly requests: number; readonly usd: number }>>
  readonly byStop: Readonly<Record<string, number>>
  readonly contextEstimated: boolean
  /** Present only when the bin's segment count fits the payload budget; never merged. */
  readonly contextSegments?: ReadonlyArray<AgentTraceContextSegment>
}

/** Timestamped lifecycle evidence associated with an agent session. */
export type AgentTraceEvent =
  | {
      readonly _tag: 'compaction'
      readonly sessionId: string
      readonly ts: number
      readonly summaryChars: number | null
    }
  | { readonly _tag: 'session_exit'; readonly sessionId: string; readonly ts: number }
  | {
      readonly _tag: 'model_change'
      readonly sessionId: string
      readonly ts: number
      readonly model: string
    }

/** A timestamped human prompt, distinguished from an in-flight steering intervention. */
export interface AgentTraceHumanPrompt {
  readonly sessionId: string
  readonly ts: number
  readonly steering: boolean
}

/** Complete trace payload for a frozen time window, including coverage and sampling metadata. */
export interface AgentTraceModel {
  readonly horizon: AgentTraceHorizon
  readonly windowStart: number // epoch ms
  readonly windowEnd: number // epoch ms ("now" for the fixture; frozen)
  readonly modelCapacities: Readonly<Record<string, number>> // token capacity by request model id
  readonly sessions: ReadonlyArray<AgentTraceSession> // sorted by startedAt
  readonly requests: ReadonlyArray<AgentTraceRequest> // retained ledger rows, sorted by ts
  readonly bins: ReadonlyArray<AgentTraceBin> // aggregate evidence for rows omitted by fixture sizing
  readonly events: ReadonlyArray<AgentTraceEvent> // sorted by ts
  readonly humanPrompts: ReadonlyArray<AgentTraceHumanPrompt>
  readonly humanPromptCount?: number
  readonly humanPromptCounts?: Readonly<Record<string, number>>
  readonly coverage: {
    readonly includedRoots: number
    readonly matchingSessions?: number
    readonly totalRoots: number
    readonly includedRequests: number
    readonly totalRequests: number
    readonly includedUsd: number
    readonly totalUsd: number
    readonly policy: string
  }
  readonly parameters: {
    readonly idleThresholdMs: 600_000
    readonly activeWithinMs: 600_000
  }
  readonly sampled: boolean // true if request rows were thinned to fit the size cap
  readonly folded: number // number of omitted request rows represented by bins
  readonly activitySampled?: boolean
}

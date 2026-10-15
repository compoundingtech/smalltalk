import { Context, Effect, Layer, Schema } from 'effect'
import type { TraceSeries } from './trace-series'
import { TraceSeriesSchema } from './trace-series-schema'
export interface SessionTraceQuery {readonly agentId:string;readonly range:'1d'|'7d'|'30d'}
export interface SessionTraceProviderService {
 readonly capabilities:{readonly series:boolean;readonly subagents:boolean;readonly history:boolean;readonly harnesses:readonly ('omp'|'claude'|'codex')[]}
 readonly load:(query:SessionTraceQuery)=>Effect.Effect<TraceSeries>
}
/** Portable SessionTraceProvider service, using the host package's Effect runtime. */
export class SessionTraceProvider extends Context.Service<SessionTraceProvider,SessionTraceProviderService>()('Fractal/SessionTraceProvider') {}

/** Host-owned source; fixtures are never loaded by this production module. */
export const sessionTraceProviderLayer = (service: SessionTraceProviderService) =>
 Layer.succeed(SessionTraceProvider, {
  capabilities: service.capabilities,
  load: Effect.fn('SessionTraceProvider.load')((query: SessionTraceQuery) =>
   service.load(query).pipe(Effect.flatMap(Schema.decodeUnknownEffect(TraceSeriesSchema)), Effect.orDie)),
 })

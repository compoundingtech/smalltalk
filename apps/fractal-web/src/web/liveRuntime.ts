import { Effect, Layer } from 'effect'
import { buildIdentity, deploymentId } from 'virtual:build-identity'
import { liveSource } from '../data/liveSource.ts'
import { makeTelemetry } from '../telemetry/browser.ts'

/** Page-owned scopes survive React Fast Refresh; a full navigation owns teardown. */
export const createPageRuntime = () => {
  // Dogfood records 100% of pages: budget roots and their SDK detail cannot disappear at 10%.
  const telemetry = makeTelemetry({
    otlpTracesUrl: import.meta.env.VITE_OTLP_TRACES_URL?.trim() || undefined,
    production: !import.meta.env.DEV,
    resourceAttributes: {
      'service.version': buildIdentity.machineVersion,
      ...(deploymentId === undefined ? {} : { 'deployment.id': deploymentId }),
      'wf.rum.sample_rate': 1,
      'wf.rum.sampling': 'dogfood-all',
    },
  })
  // Capture the mandatory reload root and its trace context synchronously, before SDK adoption
  // of the early roster socket. This does not create an exporter or wait for a committed paint.
  const ux = telemetry.ux
  // Observer lifetime is the live source's SDK scope, not a React effect or another runtime.
  const telemetryLayer = Layer.effectDiscard(
    Effect.acquireRelease(
      Effect.sync(() => telemetry.install()),
      (uninstall) => Effect.sync(uninstall),
    ),
  ).pipe(Layer.provideMerge(telemetry.layer))
  
  // The page owns one source for its lifetime; Storybook never imports this live root.
  const live = liveSource({
    options: {
      baseUrl: window.location.origin, maxFollows: 8, conversationSlots: 'advertised',
      adoptEarlyCollections: true,
      parentSpan: ux.activeSpan,
      traceContext: ux.traceContext,
    },
    telemetryLayer,
    ux: () => ux,
  })
  return { live, telemetry }
}
type PageRuntime = ReturnType<typeof createPageRuntime>
const retained = import.meta.hot?.data.pageRuntime as PageRuntime | undefined
const runtime = retained ?? createPageRuntime()
if (import.meta.hot !== undefined) import.meta.hot.data.pageRuntime = runtime
export const { live, telemetry } = runtime

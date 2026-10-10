import type { LiveSource } from '../data/liveSource.ts'

/** Socket close cannot wait for Effect scope teardown while a document is leaving. */
export const installPageLifecycle = ({ target, source, beforeDispose }: {
  readonly target: EventTarget
  readonly source: Pick<LiveSource, 'suspendSockets' | 'resumeSockets' | 'dispose'>
  /** Synchronously detach registry consumers before disposal can notify them. */
  readonly beforeDispose?: () => void
}): (() => void) => {
  const onPageHide = (event: Event) => {
    source.suspendSockets()
    if (!('persisted' in event) || event.persisted !== true) {
      beforeDispose?.()
      void source.dispose()
    }
  }
  const onPageShow = (event: Event) => {
    if ('persisted' in event && event.persisted === true) source.resumeSockets()
  }
  target.addEventListener('pagehide', onPageHide)
  target.addEventListener('pageshow', onPageShow)
  return () => {
    target.removeEventListener('pagehide', onPageHide)
    target.removeEventListener('pageshow', onPageShow)
  }
}

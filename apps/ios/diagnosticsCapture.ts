import type { ClientDiagnosticEvent } from '../../clients/typescript/st3-client';
import { redactJsError, redactReactNativeException } from './diagnosticsRedaction';

export type DiagnosticErrorHandler = (error: unknown, fatal?: boolean) => void;
export type DiagnosticRejectionOptions = { allRejections: boolean; onUnhandled: (id: number, error: unknown) => void; onHandled: (id: number) => void };
export type DiagnosticErrorRuntime = {
  ErrorUtils?: { getGlobalHandler: () => DiagnosticErrorHandler; setGlobalHandler: (handler: DiagnosticErrorHandler) => void };
  RN$registerExceptionListener?: (listener: (data: unknown) => void) => void;
};

/** Install both the Metro/ErrorUtils path and RN 0.86's C++ exception pipeline.
 * Returning rejection options lets the adapter choose Hermes vs RN Promise.
 * Forwarding keeps RN's redbox/fatal behavior without capturing its own report twice. */
export const installDiagnosticCapture = (
  runtime: DiagnosticErrorRuntime,
  retain: (payload: Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }>) => void,
  previousRejections?: DiagnosticRejectionOptions,
): DiagnosticRejectionOptions => {
  let forwarding = false;
  const retainError = (error: unknown, fatal: boolean): void => {
    try { retain(redactJsError(error, fatal)); } catch { /* Diagnostics cannot throw into RN. */ }
  };
  if (runtime.ErrorUtils) {
    const previous = runtime.ErrorUtils.getGlobalHandler();
    runtime.ErrorUtils.setGlobalHandler((error, fatal) => {
      retainError(error, fatal === true);
      forwarding = true;
      try { previous(error, fatal); } finally { forwarding = false; }
    });
  }
  runtime.RN$registerExceptionListener?.(data => {
    if (forwarding) return;
    try {
      const payload = redactReactNativeException(data);
      if (payload) retain(payload);
    } catch { /* Never invoke preventDefault or interrupt RN's exception processing. */ }
  });
  return {
    allRejections: true,
    onUnhandled: (id, error) => {
      retainError(error, false);
      forwarding = true;
      try { previousRejections?.onUnhandled(id, error); } finally { forwarding = false; }
    },
    onHandled: id => { previousRejections?.onHandled(id); },
  };
};

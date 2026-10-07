import AsyncStorage from '@react-native-async-storage/async-storage';
import * as Crypto from 'expo-crypto';
import { requireOptionalNativeModule } from 'expo';
import { AppState, Linking, Platform } from 'react-native';
import type { Capabilities, ClientDiagnosticEvent, St3Client } from '../../clients/typescript/st3-client';
import StDiagnostics from './modules/st-diagnostics';
import app from './app.json';
import { DIAGNOSTICS_KEY, DiagnosticsQueue } from './diagnosticsQueue';
import { redactJsError, safeOsVersion, safeRuntime, safeUpdateId, safeVersion } from './diagnosticsRedaction';
import { installDiagnosticCapture, type DiagnosticErrorRuntime, type DiagnosticRejectionOptions } from './diagnosticsCapture';

// RN 0.86 uses Hermes' tracker (not browser unhandledrejection). The alternate
// RN Promise implementation uses promise/setimmediate/rejection-tracking.
declare const require: (name: string) => unknown;
// RN supplies these host globals without a public TypeScript declaration.
const globals = globalThis as typeof globalThis & DiagnosticErrorRuntime & {
  HermesInternal?: { hasPromise?: () => boolean; enablePromiseRejectionTracker?: (options: DiagnosticRejectionOptions) => void };
};
const queue = new DiagnosticsQueue({ read: () => AsyncStorage.getItem(DIAGNOSTICS_KEY), write: text => AsyncStorage.setItem(DIAGNOSTICS_KEY, text) });
const updates = requireOptionalNativeModule<{ runtimeVersion?: string; updateId?: string; isEmbeddedLaunch?: boolean; isEnabled?: boolean }>('ExpoUpdates');
const fallbackLaunch = () => ({
  launch_id: Crypto.randomUUID(), started_at_unix_ms: Date.now(), app_version: app.expo.version,
  native_build: 'unknown', os_version: String(Platform.Version),
});
// A native disk/data-protection failure must not abort app startup.
const launch = (() => { try { return StDiagnostics?.launchContext() ?? fallbackLaunch(); } catch { return fallbackLaunch(); } })();
let sequence = 0x80000000;
let initialized = false;
let capturing = false;
let pending: ClientDiagnosticEvent[] = [];
let persisting = false;
let draining: Promise<void> | undefined;
let timer: ReturnType<typeof setTimeout> | undefined;
let sender: St3Client | undefined;
let generation = 0;
let active = AppState.currentState === 'active';
let connectionOwner: object | undefined;
const breadcrumbs: ClientDiagnosticEvent[] = [];
const boundaryInjections = new Set<() => void>();
const grantedClients = new WeakMap<St3Client, boolean>();

/** Cache only grants: a denial is re-discovered on the next foreground or
 * reconnect, so a later grant needs no app restart. */
export const recordDiagnosticCapabilities = (client: St3Client, caps: Capabilities): void => {
  if (caps.capabilities.some(capability => capability.id === 'write.client-diagnostics' && capability.state === 'granted')) grantedClients.set(client, true);
  else grantedClients.delete(client);
};

const scheduleUpload = (at = Date.now()): void => {
  clearTimeout(timer);
  timer = undefined;
  if (!sender || !active) return;
  const client = sender, currentGeneration = generation;
  timer = setTimeout(() => {
    timer = undefined;
    const current = () => sender === client && generation === currentGeneration && active;
    if (!current()) return;
    const flush = async (): Promise<number | undefined> => {
      if (!grantedClients.has(client)) recordDiagnosticCapabilities(client, (await client.discover()).value);
      if (!current() || grantedClients.get(client) !== true) return;
      return queue.flush(async batch => (await client.diagnosticsSubmit(batch)).value, current);
    };
    void flush().then(next => { if (current() && next !== undefined) scheduleUpload(next); })
      .catch(() => { if (current()) scheduleUpload(Date.now() + 30_000); });
  }, Math.max(0, at - Date.now()));
};

const persistPending = async (): Promise<void> => {
  if (persisting) return;
  persisting = true;
  try {
    while (pending.length) {
      const batch = pending;
      pending = [];
      try { await queue.enqueue(batch); } catch {
        // Keep bounded sanitized reports for a later lifecycle/capture retry. No
        // logging/recapture of diagnostics' own storage or transport failures.
        pending = [...batch, ...pending].slice(-128);
        return;
      }
    }
    scheduleUpload();
  } finally { persisting = false; }
};

const capture = (payload: ClientDiagnosticEvent['payload'], severity: ClientDiagnosticEvent['severity']): void => {
  if (capturing || Platform.OS !== 'ios' || sequence > 0xffffffff) return;
  capturing = true;
  try {
    const now = Date.now();
    const event: ClientDiagnosticEvent = {
      event_id: Crypto.randomUUID(), launch_id: launch.launch_id, sequence: sequence++,
      occurred_at_unix_ms: now, captured_at_unix_ms: now, occurrence_time_basis: 'exact', launch_id_basis: 'process',
      app_version: safeVersion(launch.app_version), native_build: safeVersion(launch.native_build),
      runtime_version: safeRuntime(updates?.runtimeVersion ?? `${launch.app_version}-${launch.native_build}`.slice(0, 128)),
      update_id: updates?.isEnabled && !updates.isEmbeddedLaunch ? safeUpdateId(updates.updateId) : 'embedded',
      platform: 'ios', os_version: safeOsVersion(launch.os_version), severity, capture_source: 'js', payload,
    };
    if (payload.kind === 'launch') { breadcrumbs.push(event); if (breadcrumbs.length > 16) breadcrumbs.shift(); }
    // Reuse stable breadcrumb IDs with errors so context can survive queue
    // eviction; already-ingested breadcrumbs are deduplicated by the gateway.
    if (payload.kind === 'js-error') pending = [...breadcrumbs, ...pending, event].slice(-128);
    else { pending.push(event); if (pending.length > 128) pending.shift(); }
    void persistPending().catch(() => {});
  } catch { /* Capture must never interfere with RN's original exception behavior. */ }
  finally { capturing = false; }
};

export const captureJsError = (error: unknown, fatal = false): void => {
  try { capture(redactJsError(error, fatal), fatal ? 'fatal' : 'error'); } catch { /* No diagnostic recursion. */ }
};
export const captureRootMounted = (): void => capture({ kind: 'launch', breadcrumb: 'root-mounted', inferred: false }, 'info');

export const drainNativeDiagnostics = (): Promise<void> => {
  if (draining) return draining;
  draining = (async () => {
    if (!StDiagnostics) return;
    const reports = await StDiagnostics.getPendingReports();
    const ids = await queue.enqueue(reports);
    // Native ownership transfers only after the JS storage write succeeds.
    if (ids.length) await StDiagnostics.acknowledge(ids);
    scheduleUpload();
  })().catch(() => {}).finally(() => { draining = undefined; });
  return draining;
};

/** Credential is only a readiness signal. The existing client retains its in-memory
 * Keychain-hydrated supplier; no credential/URL is copied into diagnostic storage. */
export const connectDiagnostics = (owner: object, client: St3Client | null, url: string, credential: string | null, enabled: boolean): void => {
  connectionOwner = owner;
  generation++;
  sender = enabled && url && credential && client ? client : undefined;
  scheduleUpload();
  void persistPending().catch(() => {});
  void drainNativeDiagnostics();
};
export const disconnectDiagnostics = (owner: object): void => {
  if (connectionOwner !== owner) return;
  connectionOwner = undefined; generation++; sender = undefined;
  clearTimeout(timer);
  timer = undefined;
};

export const subscribeDiagnosticBoundaryInjection = (listener: () => void): (() => void) => {
  if (!__DEV__) return () => {};
  boundaryInjections.add(listener);
  return () => { boundaryInjections.delete(listener); };
};
const injectDiagnostic = (link: string | null): void => {
  if (!__DEV__ || !link) return;
  let mode: string | null;
  try {
    const url = new URL(link);
    if (!/^com\.compoundingtech\.smalltalk\.starter:$/.test(url.protocol) || url.hostname !== 'diagnostics') return;
    mode = url.searchParams.get('mode');
  } catch { return; }
  if (mode === 'global') setTimeout(() => { throw new TypeError('Diagnostic injection: private message https://invalid.example/private?token=redact'); }, 0);
  else if (mode === 'rejection') void Promise.reject(new RangeError('Diagnostic injection: private rejection'));
  else if (mode === 'boundary') boundaryInjections.forEach(listener => listener());
};

export const handleInitialDiagnosticLink = (): void => {
  if (__DEV__) void Linking.getInitialURL().then(injectDiagnostic).catch(() => {});
};

export const initializeDiagnostics = (): void => {
  if (initialized || Platform.OS !== 'ios') return;
  initialized = true;
  // These are RN's own options, preserving its Debug warning/handled behavior.
  const rnRejections = __DEV__ ? require('react-native/Libraries/promiseRejectionTrackingOptions') as { default: DiagnosticRejectionOptions } : undefined;
  const options = installDiagnosticCapture(globals, payload => capture(payload, payload.fatal ? 'fatal' : 'error'), rnRejections?.default);
  if (globals.HermesInternal?.hasPromise?.()) globals.HermesInternal.enablePromiseRejectionTracker?.(options);
  else {
    // This is the same tracker RN installs when Hermes promises are unavailable.
    const tracker = require('promise/setimmediate/rejection-tracking') as { enable: (options: DiagnosticRejectionOptions) => void };
    tracker.enable(options);
  }
  capture({ kind: 'launch', breadcrumb: 'js-start', inferred: false }, 'info');
  if (active) capture({ kind: 'launch', breadcrumb: 'foreground', inferred: false }, 'info');
  void drainNativeDiagnostics();
  AppState.addEventListener('change', state => {
    const foreground = state === 'active';
    if (foreground !== active) {
      active = foreground;
      capture({ kind: 'launch', breadcrumb: foreground ? 'foreground' : 'background', inferred: false }, 'info');
    }
    if (foreground) { void persistPending().catch(() => {}); void drainNativeDiagnostics(); }
    scheduleUpload();
  });
  if (__DEV__) {
    // Cold-start injection is postponed until the boundary has mounted.
    Linking.addEventListener('url', event => injectDiagnostic(event.url));
  }
};
initializeDiagnostics();

import type { ClientDiagnosticEvent } from '../../clients/typescript/st3-client';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const TOKEN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
const names = ['Error', 'TypeError', 'RangeError', 'ReferenceError', 'SyntaxError', 'UnknownError'] as const;
const breadcrumbs = ['native-start', 'js-start', 'root-mounted', 'foreground', 'background', 'previous-launch-unclean'] as const;
const severities = ['info', 'warning', 'error', 'fatal'] as const;
const sources = ['js', 'native-marker', 'metrickit'] as const;
const modules = ['app', 'react-native', 'unknown'] as const;
const binaries = ['app', 'system', 'unknown'] as const;
const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
const member = <T extends string>(values: readonly T[], value: unknown): value is T => typeof value === 'string' && values.some(item => item === value);
const integer = (value: unknown, max = Number.MAX_SAFE_INTEGER): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= max;
const nullableU32 = (value: unknown): value is number | null => value === null || integer(value, 0xffffffff);
const token = (value: unknown, max: number, pattern: RegExp): value is string => typeof value === 'string' && value.length <= max && pattern.test(value);
export const safeVersion = (value: unknown, max = 64): string => token(value, max, TOKEN) ? value : 'unknown';
export const safeRuntime = (value: unknown): string => token(value, 128, TOKEN) ? value : 'unknown';
export const safeOsVersion = (value: unknown): string => token(value, 32, TOKEN) ? value : 'unknown';
export const safeUpdateId = (value: unknown): string => typeof value === 'string' && UUID.test(value) ? value.toLowerCase() : 'embedded';

/** Construct a new allowlisted object, never persist arbitrary report keys. */
export const redactDiagnosticEvent = (input: unknown): ClientDiagnosticEvent | undefined => {
  const event = input;
  if (!record(event) || typeof event.event_id !== 'string' || !UUID.test(event.event_id) || typeof event.launch_id !== 'string' || !UUID.test(event.launch_id)
    || !integer(event.sequence, 0xffffffff) || !integer(event.occurred_at_unix_ms) || !integer(event.captured_at_unix_ms)
    || event.platform !== 'ios' || !member(severities, event.severity) || !member(sources, event.capture_source)) return;
  if (!member(['exact', 'metric-interval-end'] as const, event.occurrence_time_basis) || !member(['process', 'metric-interval'] as const, event.launch_id_basis)) return;
  const payload = event.payload;
  if (!record(payload)) return;
  let clean: ClientDiagnosticEvent['payload'];
  if (payload.kind === 'launch') {
    if (!member(breadcrumbs, payload.breadcrumb) || typeof payload.inferred !== 'boolean') return;
    clean = { kind: 'launch', breadcrumb: payload.breadcrumb, inferred: payload.inferred };
  } else if (payload.kind === 'js-error') {
    if (!member(names, payload.name) || typeof payload.fatal !== 'boolean' || !Array.isArray(payload.frames)) return;
    const frames: Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }>['frames'] = [];
    for (const value of payload.frames.slice(0, 32)) {
      const frame = value;
      if (record(frame) && member(modules, frame.module) && integer(frame.line, 0xffffffff) && integer(frame.column, 0xffffffff)) frames.push({ module: frame.module, line: frame.line, column: frame.column });
    }
    clean = { kind: 'js-error', name: payload.name, fatal: payload.fatal, frames };
  } else if (payload.kind === 'native-crash' || payload.kind === 'hang') {
    if (!Array.isArray(payload.frames)) return;
    const frames: Extract<ClientDiagnosticEvent['payload'], { kind: 'hang' }>['frames'] = [];
    for (const value of payload.frames.slice(0, 32)) {
      const frame = value;
      if (record(frame) && member(binaries, frame.binary) && token(frame.offset, 32, /^[0-9a-f]+$/)) frames.push({ binary: frame.binary, offset: frame.offset });
    }
    if (payload.kind === 'hang') {
      if (typeof payload.duration_ms !== 'number' || !Number.isFinite(payload.duration_ms) || payload.duration_ms < 0) return;
      clean = { kind: 'hang', duration_ms: payload.duration_ms, frames };
    } else {
      if (!nullableU32(payload.exception_type) || !nullableU32(payload.signal)) return;
      clean = { kind: 'native-crash', exception_type: payload.exception_type, signal: payload.signal, frames };
    }
  } else return;
  return {
    event_id: event.event_id.toLowerCase(), launch_id: event.launch_id.toLowerCase(), sequence: event.sequence,
    occurred_at_unix_ms: event.occurred_at_unix_ms, captured_at_unix_ms: event.captured_at_unix_ms,
    occurrence_time_basis: event.occurrence_time_basis, launch_id_basis: event.launch_id_basis,
    app_version: safeVersion(event.app_version), native_build: safeVersion(event.native_build),
    runtime_version: safeRuntime(event.runtime_version), update_id: safeUpdateId(event.update_id), platform: 'ios',
    os_version: safeOsVersion(event.os_version), severity: event.severity, capture_source: event.capture_source, payload: clean,
  };
};

const frameModule = (location: string): 'app' | 'react-native' | 'unknown' =>
  /(?:^|\/)node_modules\/react-native\//.test(location) ? 'react-native'
    : /(?:^|\/)(?:index\.bundle|main\.jsbundle|App\.[jt]sx?)(?:\?|$)/.test(location) || /\/apps\/ios\//.test(location) ? 'app' : 'unknown';

/** Read only Error name/stack; stack text is consumed locally and never retained. */
export const redactJsError = (error: unknown, fatal: boolean): Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }> => {
  let name: unknown;
  let stack: unknown;
  try { if (error instanceof Error) { name = error.name; stack = error.stack; } } catch { /* hostile getters cannot break capture */ }
  const frames: Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }>['frames'] = [];
  if (typeof stack === 'string') {
    // Hermes (fn@file:line:column) and Metro/V8 (at fn (file:line:column)).
    // Ignore the first line: it commonly contains the private error message.
    for (const line of stack.slice(0, 16_384).split('\n').slice(1, 65)) {
      const match = /(?:@|\(|\s)([^\s()]+):(\d+):(\d+)\)?$/.exec(line.trim());
      if (!match) continue;
      const row = Number(match[2]), column = Number(match[3]);
      if (!integer(row, 0xffffffff) || !integer(column, 0xffffffff)) continue;
      frames.push({ module: frameModule(match[1]), line: row, column });
      if (frames.length === 32) break;
    }
  }
  return { kind: 'js-error', name: member(names, name) ? name : 'UnknownError', fatal, frames };
};

/** The RN 0.86 C++ exception listener supplies parsed frames rather than Error.
 * Ignore its message, componentStack, extraData, rawStack, and preventDefault. */
export const redactReactNativeException = (input: unknown): Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }> | undefined => {
  const error = input;
  if (!record(error) || typeof error.isFatal !== 'boolean' || !Array.isArray(error.stack)) return;
  const frames: Extract<ClientDiagnosticEvent['payload'], { kind: 'js-error' }>['frames'] = [];
  for (const value of error.stack.slice(0, 32)) {
    const frame = value;
    if (!record(frame) || !integer(frame.lineNumber, 0xffffffff) || !integer(frame.column, 0xffffffff)) continue;
    frames.push({ module: typeof frame.file === 'string' ? frameModule(frame.file.slice(0, 2048)) : 'unknown', line: frame.lineNumber, column: frame.column });
  }
  return { kind: 'js-error', name: member(names, error.name) ? error.name : 'UnknownError', fatal: error.isFatal, frames };
};

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { redactDiagnosticEvent, redactJsError, redactReactNativeException, safeOsVersion, safeRuntime, safeUpdateId, safeVersion } from './diagnosticsRedaction.ts';
import { installDiagnosticCapture } from './diagnosticsCapture.ts';

const uuid = '12345678-1234-8234-8234-123456789abc';
const event = {
  event_id: uuid, launch_id: uuid, sequence: 0xffffffff, occurred_at_unix_ms: 1000, captured_at_unix_ms: 1001,
  occurrence_time_basis: 'exact', launch_id_basis: 'process', app_version: '1.0.0', native_build: '42', runtime_version: 'ios-42', update_id: 'embedded',
  platform: 'ios', os_version: '27.0', severity: 'error', capture_source: 'js',
  payload: { kind: 'js-error', name: 'TypeError', fatal: false, frames: [{ module: 'app', line: 12, column: 34 }] },
};

test('metadata is bounded ASCII tokens, never URLs/paths/user strings', () => {
  for (const value of ['https://secret.example/path?token=x', '/work/private/file', 'private message', '', 'a+b', '😀', 'x'.repeat(129)]) {
    assert.equal(safeVersion(value), 'unknown');
    assert.equal(safeRuntime(value), 'unknown');
    assert.equal(safeOsVersion(value), 'unknown');
  }
  assert.equal(safeVersion('1.2.3-beta_42'), '1.2.3-beta_42');
  assert.equal(safeRuntime('x'.repeat(128)), 'x'.repeat(128));
  assert.equal(safeVersion('x'.repeat(65)), 'unknown');
  assert.equal(safeOsVersion('x'.repeat(33)), 'unknown');
  assert.equal(safeUpdateId(uuid.toUpperCase()), uuid);
  assert.equal(safeUpdateId('https://private.example'), 'embedded');
});

test('Expo nativeVersion runtime survives JS and native report redaction', () => {
  assert.equal(safeRuntime('0.1.0(1)'), '0.1.0(1)');
  for (const capture_source of ['js', 'native-marker']) {
    const input = { ...event, capture_source, runtime_version: '0.1.0(1)', payload: { kind: 'launch', breadcrumb: capture_source === 'js' ? 'js-start' : 'native-start', inferred: false } };
    assert.equal(redactDiagnosticEvent(input).runtime_version, '0.1.0(1)');
  }
  for (const value of ['0.1.0(1)/private', '0.1.0(secret message)', '0.1.0(1)\n', `${'x'.repeat(126)}(1)`]) {
    assert.equal(safeRuntime(value), 'unknown');
    assert.equal(redactDiagnosticEvent({ ...event, runtime_version: value }).runtime_version, 'unknown');
  }
});

test('event redaction rebuilds exact allowlisted payload and frame fields', () => {
  const clean = redactDiagnosticEvent({ ...event, headers: { Authorization: 'Bearer secret' }, message: 'private', path: '/work/private',
    payload: { ...event.payload, message: 'secret', stack: 'private stack', arbitrary: 'private', frames: [{ module: 'app', line: 12, column: 34, url: 'https://secret', method: 'user-string' }] } });
  assert.deepEqual(clean, event);
  assert.equal(JSON.stringify(clean).includes('secret'), false);
  assert.equal(redactDiagnosticEvent({ ...event, event_id: 'not-a-uuid' }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, sequence: 0x100000000 }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, occurred_at_unix_ms: -1 }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, platform: 'android' }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, occurrence_time_basis: undefined }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, launch_id_basis: 'unknown' }), undefined);
});

test('JS frames are bounded numeric locations; messages and stack text stay local', () => {
  const error = new TypeError('Authorization: Bearer secret https://private.example/user');
  error.stack = 'TypeError: private\nfn@http://localhost:8081/index.bundle?token=secret:12:34\n    at mount (/work/private/apps/ios/App.tsx:56:78)\n    at rn (/work/private/node_modules/react-native/a.js:90:12)\n    at unknown (https://private.example/x.js:4:5)\n' + 'f@address at 0x123:1:2\n'.repeat(100);
  const clean = redactJsError(error, true);
  assert.equal(clean.name, 'TypeError');
  assert.equal(clean.fatal, true);
  assert.deepEqual(clean.frames.slice(0, 4), [{ module: 'app', line: 12, column: 34 }, { module: 'app', line: 56, column: 78 }, { module: 'react-native', line: 90, column: 12 }, { module: 'unknown', line: 4, column: 5 }]);
  assert.ok(clean.frames.length <= 32);
  assert.equal(JSON.stringify(clean).includes('private'), false);
  assert.equal(JSON.stringify(clean).includes('secret'), false);
  assert.deepEqual(redactJsError('arbitrary private string', false), { kind: 'js-error', name: 'UnknownError', fatal: false, frames: [] });
  const hostile = new Error(); Object.defineProperty(hostile, 'stack', { get() { throw new Error('getter'); } });
  assert.deepEqual(redactJsError(hostile, false).frames, []);
});

test('native crash/hang summaries strip extra keys and invalid frames', () => {
  const crash = redactDiagnosticEvent({ ...event, capture_source: 'metrickit', occurrence_time_basis: 'metric-interval-end', launch_id_basis: 'metric-interval',
    payload: { kind: 'native-crash', exception_type: 1, signal: 11, message: 'private', frames: [
      { binary: 'app', offset: 'abcdef', path: '/private' }, { binary: 'system', offset: 'ABC' }, { binary: 'user-string', offset: '1' }, { binary: 'unknown', offset: 'https://private' },
    ] } });
  assert.deepEqual(crash.payload, { kind: 'native-crash', exception_type: 1, signal: 11, frames: [{ binary: 'app', offset: 'abcdef' }] });
  const hang = redactDiagnosticEvent({ ...event, payload: { kind: 'hang', duration_ms: 123.5, frames: Array.from({ length: 100 }, () => ({ binary: 'system', offset: '0' })) } });
  assert.equal(hang.payload.frames.length, 32);
  assert.equal(redactDiagnosticEvent({ ...event, payload: { kind: 'hang', duration_ms: Infinity, frames: [] } }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, payload: { kind: 'launch', breadcrumb: 'user-string', inferred: false } }), undefined);
});

test('RN 0.86 parsed exception frames are sanitized without using message/extraData', () => {
  const clean = redactReactNativeException({ name: 'RangeError', isFatal: true, message: 'private', componentStack: 'private', extraData: { rawStack: 'private' }, stack: [
    { file: '/work/private/apps/ios/App.tsx', methodName: 'private', lineNumber: 8, column: 9 },
    { file: 'https://private.example', lineNumber: -1, column: 3 },
  ] });
  assert.deepEqual(clean, { kind: 'js-error', name: 'RangeError', fatal: true, frames: [{ module: 'app', line: 8, column: 9 }] });
});

test('global handler and native listener preserve RN forwarding without duplicate capture', () => {
  const reports = []; const forwarded = []; let listener; let handler;
  const original = (error, fatal) => { forwarded.push([error, fatal]); listener({ name: error.name, isFatal: !!fatal, stack: [] }); };
  handler = original;
  const options = installDiagnosticCapture({ ErrorUtils: { getGlobalHandler: () => handler, setGlobalHandler: next => { handler = next; } }, RN$registerExceptionListener: next => { listener = next; } }, payload => reports.push(payload), {
    allRejections: true, onUnhandled: (id, error) => original(error, false), onHandled: id => forwarded.push(id),
  });
  const error = new TypeError('private'); handler(error, true);
  assert.equal(reports.length, 1); assert.deepEqual(forwarded[0], [error, true]);
  listener({ name: 'Error', isFatal: false, stack: [] }); assert.equal(reports.length, 2);
  options.onUnhandled(1, new RangeError('private')); assert.equal(reports.length, 3); assert.equal(reports[2].fatal, false);
  options.onHandled(1); assert.equal(forwarded.at(-1), 1);
  assert.equal(options.allRejections, true);
});

test('diagnostic capture failure does not suppress or alter RN fatal behavior', () => {
  let handler; const error = new Error('original');
  installDiagnosticCapture({ ErrorUtils: { getGlobalHandler: () => () => { throw error; }, setGlobalHandler: next => { handler = next; } } }, () => { throw new Error('diagnostics storage unavailable'); });
  assert.throws(() => handler(error, true), value => value === error);
});

test('Release rejection capture works without RN Debug callbacks', () => {
  const reports = [];
  const options = installDiagnosticCapture({}, payload => reports.push(payload));
  options.onUnhandled(1, new ReferenceError('private'));
  options.onHandled(1);
  assert.equal(reports.length, 1);
  assert.equal(reports[0].name, 'ReferenceError');
  assert.equal(reports[0].fatal, false);
});

test('native exception and signal accept nullable u32 values only', () => {
  const payload = { kind: 'native-crash', exception_type: null, signal: 0xffffffff, frames: [] };
  assert.deepEqual(redactDiagnosticEvent({ ...event, payload }).payload, payload);
  assert.equal(redactDiagnosticEvent({ ...event, payload: { ...payload, signal: -1 } }), undefined);
  assert.equal(redactDiagnosticEvent({ ...event, payload: { ...payload, exception_type: 0x100000000 } }), undefined);
});

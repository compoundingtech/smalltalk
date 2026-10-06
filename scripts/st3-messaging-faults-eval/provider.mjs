// A token-free omp API stand-in. Load the candidate binary's actual extension;
// only the provider API and model turn are replaced. Never poll for messages.
import fs from 'node:fs';
import http from 'node:http';
import childProcess from 'node:child_process';
import { pathToFileURL } from 'node:url';

if (process.argv.includes('--version')) {
  console.log('omp v18.4.4');
  process.exit(0);
}
const directory = process.cwd();
// Exercise an actual historical channel with today's daemon/driver/extension,
// avoiding unrelated historical daemon startup and prompt contracts.
if (process.env.FAULT_OLD_CHANNEL_BIN) {
  // The provider may load either generation's immutable extension during compatibility tests.
  process.env.ST_OMP_CHANNEL_BIN = process.env.FAULT_OLD_CHANNEL_BIN;
  process.env.ST2_OMP_CHANNEL_BIN = process.env.FAULT_OLD_CHANNEL_BIN;
}
const actor = process.env.ST_AGENT;
const endpoint = process.env.ST3_ENDPOINT;
const events = new Map();
const record = (value) => fs.appendFileSync(`${directory}/receipts.jsonl`,
  JSON.stringify({ at_unix_ms: Date.now(), pid: process.pid, ...value }) + '\n');
process.on('uncaughtException', error => { record({ event: 'error', error: String(error.stack) }); process.exit(1); });
process.on('unhandledRejection', error => { record({ event: 'error', error: String(error.stack) }); process.exit(1); });
const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));
// Old channel versions put the immutable ID only in frame metadata. Observe
// that metadata without changing the bytes given to the actual extension.
// A wire frame is NOT a receipt; only sendUserMessage below records consumption.
const pendingFrames = [];
const children = new Set();
const killChannel = (child) => {
  if (!child.pid) return; // spawn can fail before assigning a PID
  try { process.kill(-child.pid, 'SIGKILL'); } catch (error) {
    if (error.code !== 'ESRCH') throw error;
  }
};
// The extension's channels get private groups. This synchronous exit hook also
// covers startup errors and rejected shutdown hooks; the eval guardian covers
// SIGKILL of the provider or controller, when no JavaScript hook can run.
process.on('exit', () => {
  for (const child of children) killChannel(child);
});
const spawn = childProcess.spawn;
childProcess.spawn = (...args) => {
  if (args[0] === process.env.FAULT_OLD_CHANNEL_BIN && process.env.ST_DRIVER_ROOT
      && !args[1].includes('--catalog')) {
    // This pinned graph channel only checks that the obsolete flag is present; it never
    // reads a catalog. Adapt its CLI in the fixture, using the real native root without
    // fabricating declarations or restoring catalog arguments in the current extension.
    args[1] = ['--catalog', process.env.ST_DRIVER_ROOT, ...args[1]];
  }
  args[2] = { ...args[2], detached: true };
  const child = spawn(...args);
  children.add(child);
  child.on('close', () => {
    killChannel(child);
    children.delete(child);
  });
  let partial = '';
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', chunk => {
    partial += chunk;
    let end;
    while ((end = partial.indexOf('\n')) >= 0) {
      const line = partial.slice(0, end);
      partial = partial.slice(end + 1);
      try {
        const frame = JSON.parse(line);
        if (frame.type === 'message' && typeof frame.content === 'string' && typeof frame.meta?.messageId === 'string') {
          pendingFrames.push({ subject: frame.meta.messageId, content: frame.content });
          record({ event: 'wire-frame', subject: frame.meta.messageId });
        }
      } catch { /* the real extension handles the same frame */ }
    }
  });
  return child;
};
const post = (path, body) => new Promise((resolve, reject) => {
  const bytes = JSON.stringify(body);
  const request = http.request({ socketPath: endpoint, path, method: 'POST',
    headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(bytes) } }, response => {
    let text = '';
    response.on('data', chunk => { text += chunk; });
    response.on('end', () => response.statusCode === 200 ? resolve(JSON.parse(text))
      : reject(new Error(`${response.statusCode}: ${text}`)));
  });
  request.on('error', reject);
  request.setTimeout(1000, () => request.destroy(new Error('receipt timeout')));
  request.end(bytes);
});
const ctx = {
  isIdle: () => true,
  sessionManager: { getSessionId: () => `fault-provider-${process.pid}`, getEntries: () => [] },
  ui: { notify: (message, level) => record({ event: 'notification', message, level }) },
};
let acknowledgements = Promise.resolve();
const api = {
  on: (event, callback) => events.set(event, callback),
  sendMessage: () => {},
  setSessionName: (label) => record({ event: 'seat-title', label }),
  sendUserMessage: async (content) => {
    // Join consumed provider text to the real channel's immutable metadata.
    const matched = pendingFrames.filter(frame => content.includes(frame.content));
    const ids = matched.map(frame => frame.subject);
    for (const frame of matched) pendingFrames.splice(pendingFrames.indexOf(frame), 1);
    if (!ids.length) throw new Error('native message has no immutable envelope id');
    if (fs.existsSync(`${directory}/handoff-blocked`)) {
      for (const subject of ids) record({ event: 'handoff-refused', subject });
      throw new Error('fixture native handoff is temporarily unavailable');
    }
    for (const subject of ids) {
      record({ event: 'received', subject, content });
      // Reading is the stand-in's model action, after native delivery. Keep a
      // durable receipt before acknowledging; record every duplicate handoff.
      acknowledgements = acknowledgements.then(async () => {
        for (const lifecycle of ['delivered', 'read', 'closed']) {
          for (;;) {
            try {
              const claim = await post(`/v1/messages/${subject.slice(8)}/claims`, {
                lifecycle, actor, idempotency_key: `fault-reader:${subject}:${lifecycle}`,
              });
              record({ event: lifecycle, subject, accepted_at_unix_ms: (claim.value ?? claim).accepted_at_unix_ms });
              break;
            } catch (error) {
              record({ event: 'ack-retry', subject, lifecycle, error: String(error) });
              await sleep(200);
            }
          }
        }
      });
    }
  },
};
record({ event: 'started' });
const index = process.argv.findIndex(arg => arg === '--extension' || arg === '-e');
if (index < 0) throw new Error('native driver did not supply its extension');
const { default: extension } = await import(pathToFileURL(process.argv[index + 1]));
extension(api);
await events.get('session_start')({}, ctx);
record({ event: 'ready' });
const keepalive = setInterval(() => {}, 1000);
let shuttingDown = false;
for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(signal, async () => {
  if (shuttingDown) return;
  shuttingDown = true;
  // A stuck extension must not hold the driver in provider shutdown indefinitely.
  const deadline = setTimeout(() => process.exit(1), 1000);
  try {
    await events.get('session_shutdown')?.({}, ctx);
  } finally {
    clearTimeout(deadline);
    clearInterval(keepalive);
    process.exit(0);
  }
});

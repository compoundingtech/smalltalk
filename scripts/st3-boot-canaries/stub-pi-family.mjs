// A token-free `pi` or `omp` for the boot canaries. st's real extension (-e FILE) runs unchanged in
// a minimal host; only the provider API and the model turn are replaced. The host does what pi does
// at each boundary: starts the session, hands the extension idle state, takes native user messages
// and raises the `context` event for them, and acts on a message as a model would.
import childProcess from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const harness = process.env.STUB_HARNESS;
if (process.argv.includes('--version')) {
  // omp's launcher admits only some releases; pi has no version gate.
  console.log(harness === 'omp' ? 'omp v18.4.4' : 'pi 0.0.0-stub');
  process.exit(0);
}
const agent = process.env.ST_AGENT ?? 'unknown';
const receiptPath = `${process.cwd()}/receipts-${agent.replace(/[^A-Za-z0-9]/g, '-')}.jsonl`;
const record = (event, fields = {}) => fs.appendFileSync(receiptPath,
  JSON.stringify({ event, at: Date.now() / 1000, pid: process.pid, ...fields }) + '\n');
const st = (...args) => childProcess.spawnSync(process.env.ST3_BIN, args, { encoding: 'utf8', timeout: 60000 });

// What a model does when woken: read the message, then run the command the wake gives, exactly as
// written. Nothing is added: a wake a model cannot follow verbatim fails the canary.
const act = (content) => {
  const message = content.match(/graph="(message\/[0-9a-f]+)"/) ?? content.match(/message\/[0-9a-f]+/);
  const command = content.match(/Run `(st work claim [^`]+)`/);
  if (message) {
    const reference = message[1] ?? message[0];
    const result = st('conversations', 'read', reference, '--as', agent, '--json');
    record('read', { reference, exit: result.status, stderr: (result.stderr ?? '').slice(-1000) });
  }
  if (command) {
    const result = st(...command[1].split(/\s+/).slice(1));
    record('claim', { command: command[1], exit: result.status, stderr: (result.stderr ?? '').slice(-1000) });
  }
};

record('started', { argv: process.argv.slice(2), harness });

// The session, kept as pi and omp keep it: `<time>_<id>.jsonl` in `--session-dir`, whose first line
// names the session. pi resumes `--session PATH` (and starts a new session at a path that does not
// exist); omp resumes `--resume ID` and refuses an ID it has no transcript for.
const option = (name) => {
  const index = process.argv.indexOf(name);
  return index > 0 ? process.argv[index + 1] : undefined;
};
const sessionDir = option('--session-dir') ?? process.cwd();
const headerId = (file) => JSON.parse(fs.readFileSync(file, 'utf8').split('\n')[0]).id;
let sessionFile;
let sessionId;
if (harness === 'omp' && option('--resume')) {
  sessionId = option('--resume');
  const name = fs.readdirSync(sessionDir).find((entry) => entry.endsWith(`_${sessionId}.jsonl`));
  if (!name) {
    console.error(`Session ${sessionId} not found`);
    record('session-missing', { sessionId });
    process.exit(1);
  }
  sessionFile = path.join(sessionDir, name);
} else if (harness === 'pi' && option('--session') && fs.existsSync(option('--session'))) {
  sessionFile = option('--session');
  sessionId = headerId(sessionFile);
} else {
  sessionId = crypto.randomUUID();
  sessionFile = option('--session') ?? path.join(sessionDir, `${Date.now()}_${sessionId}.jsonl`);
  fs.mkdirSync(path.dirname(sessionFile), { recursive: true });
  fs.writeFileSync(sessionFile, JSON.stringify({ type: 'session', id: sessionId, cwd: process.cwd() }) + '\n');
}
const resumed = fs.readFileSync(sessionFile, 'utf8').split('\n').length > 2;
fs.appendFileSync(sessionFile, JSON.stringify({ type: 'launch', at: Date.now() }) + '\n');
record('session', { sessionId, sessionFile, resumed });
const events = new Map();
let title = '';
const ctx = {
  isIdle: () => true,
  getAsyncJobSnapshot: () => ({ running: [] }),
  sessionManager: { getSessionId: () => sessionId, getSessionFile: () => sessionFile, getEntries: () => [] },
  ui: { notify: (message, level) => record('notification', { message, level }) },
};
const api = {
  on: (event, callback) => events.set(event, callback),
  sendMessage: () => {},
  setSessionName: (label) => { title = label; record('seat-title', { label }); },
  // The native handoff: the provider takes the text as a user turn, raises `context` with it, and
  // the model answers. Delivered and read evidence come from the extension, not from here.
  sendUserMessage: async (content) => {
    record('turn', { text: content });
    await events.get('context')?.({ messages: [{ role: 'user', content }] }, ctx);
    setTimeout(() => act(content), 0);
  },
};
const index = process.argv.findIndex((arg) => arg === '--extension' || arg === '-e');
if (index < 0) throw new Error('the driver did not supply its extension');
const { default: extension } = await import(pathToFileURL(process.argv[index + 1]));
extension(api);
await events.get('session_start')?.({}, ctx);
record('ready');
const keepalive = setInterval(() => {}, 1000);
for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP']) {
  process.on(signal, async () => {
    // pi's extension closes its channel only for a quit.
    await events.get('session_shutdown')?.({ reason: 'quit' }, ctx);
    clearInterval(keepalive);
    process.exit(0);
  });
}

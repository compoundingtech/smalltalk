const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const { createHash } = require('node:crypto');
const { spawn } = require('node:child_process');
const ts = require('../../../apps/ios/node_modules/typescript');

// Runs against actual IndexedDB and WebSocket implementations, not a storage/socket mock.
const browserChecks = async () => {
    const { IndexedDbCredentialStore, St3Client } = await import('/index');
    const gateway = location.origin;
    const otherGateway = 'https://other-gateway.example';
    const store = new IndexedDbCredentialStore();
    const equal = (actual, expected) => {
        if (actual !== expected) throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
    };
    if (!location.search) {
        equal(await store.get(gateway), undefined);
        const client = new St3Client({ baseUrl: gateway });
        const paired = await client.completePairing('pairing/test', { api_version: 'st3.client.v0', code: 'ABCDEFGH', device_public_key: 'browser-test-device' });
        await store.set(gateway, paired.value.credential);
        await store.set(otherGateway, 'other-bearer');
        await store.set(`${gateway}/nested`, 'nested-bearer');
        location.search = '?resume';
        return false;
    }
    // Navigation destroys the first store/client; IndexedDB must retain the native bearer.
    equal(await store.get(`${gateway}/`), 'paired-native-bearer');
    equal(await store.get(otherGateway), 'other-bearer');
    equal(await store.get(`${gateway}/nested/`), 'nested-bearer');
    equal(await store.get('http://other-gateway.example'), undefined);
    equal(await store.get(`${gateway}/other`), undefined);
    const client = new St3Client({ baseUrl: gateway, credential: () => store.get(gateway) });
    equal((await client.capabilities()).value.authenticated, true);
    const receive = async (open) => {
        let resolve, reject;
        const received = new Promise((yes, no) => { resolve = yes; reject = no; });
        const stream = await open(resolve, error => reject(error ?? new Error('Stream ended before its first frame')));
        await received;
        stream.close();
    };
    await receive((onScreen, onEnd) => client.terminalStream('terminal/release-shell', { streamCapability: 'attach-proof', incarnation: 'pty-1', onScreen, onEnd }));
    await receive((onChange, onEnd) => client.conversationStream('session/example', { after: 'cursor/example', onChange, onEnd }));
    await receive((onFrame, onEnd) => client.collectionStream({ onFrame, onEnd }));
    await store.set(gateway, 'replacement-bearer');
    equal(await new IndexedDbCredentialStore().get(gateway), 'replacement-bearer');
    await store.delete(`${gateway}/`);
    equal(await store.get(gateway), undefined);
    equal((await client.capabilities()).value.authenticated, false);
    equal(await store.get(otherGateway), 'other-bearer');
    equal(await store.get(`${gateway}/nested`), 'nested-bearer');
    await store.delete(gateway); // Disconnect/revocation cleanup is idempotent.
    await store.delete(otherGateway);
    await store.delete(`${gateway}/nested`);
    return true;
};

const frame = (value) => {
    const payload = Buffer.from(JSON.stringify(value));
    const header = Buffer.alloc(payload.length < 126 ? 2 : 4);
    header[0] = 0x81;
    header[1] = payload.length < 126 ? payload.length : 126;
    if (payload.length >= 126) header.writeUInt16BE(payload.length, 2);
    return Buffer.concat([header, payload]);
};

test('browser persists gateway-scoped native bearers and authenticates every stream without URL credentials', { timeout: 30000 }, async () => {
    const modules = new Map();
    for (const name of ['Models.generated', 'Client.generated', 'errors', 'IndexedDbCredentialStore', 'index']) {
        const source = fs.readFileSync(path.join(__dirname, `${name}.ts`), 'utf8');
        modules.set(`/${name}`, ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 } }).outputText);
    }
    const snapshot = { id: 'snapshot/test', host_id: 'host/test', store_index: 1, projection_version: 'client-projection.v0', created_at: '2026-09-20T00:00:00Z' };
    const envelope = value => ({ api_version: 'st3.client.v0', request_id: 'request/test', snapshot, value });
    const upgrades = [];
    const sockets = new Set();
    let resolveResult, rejectResult;
    const result = new Promise((resolve, reject) => { resolveResult = resolve; rejectResult = reject; });
    const server = http.createServer(async (request, response) => {
        response.setHeader('Content-Security-Policy', "default-src 'self'; script-src 'self'; connect-src 'self'");
        if (request.url === '/result') {
            let body = '';
            for await (const chunk of request) body += chunk;
            response.end();
            resolveResult(JSON.parse(body));
        } else if (request.url === '/checks.js') {
            response.setHeader('Content-Type', 'text/javascript');
            response.end(`(${browserChecks.toString()})().then(done => { if (done) return fetch('/result', {method: 'POST', body: JSON.stringify({ok: true})}); }).catch(error => fetch('/result', {method: 'POST', body: JSON.stringify({error: String(error.stack ?? error)})}));`);
        } else if (modules.has(request.url)) {
            response.setHeader('Content-Type', 'text/javascript');
            response.end(modules.get(request.url));
        } else if (request.url === '/v1/client/pairings/test/complete') {
            response.setHeader('Content-Type', 'application/json');
            response.end(JSON.stringify(envelope({ kind: 'paired-session', credential: 'paired-native-bearer' })));
        } else if (request.url === '/v1/client/capabilities') {
            response.setHeader('Content-Type', 'application/json');
            response.end(JSON.stringify(envelope({ authenticated: request.headers.authorization === 'Bearer paired-native-bearer' })));
        } else {
            response.setHeader('Content-Type', 'text/html');
            response.end('<!doctype html><title>st3 browser integration</title><script type="module" src="/checks.js"></script>');
        }
    });
    server.on('upgrade', (request, socket) => {
        sockets.add(socket);
        socket.on('close', () => sockets.delete(socket));
        const protocols = request.headers['sec-websocket-protocol']?.split(',').map(value => value.trim()) ?? [];
        upgrades.push({ url: request.url, protocols, authorization: request.headers.authorization });
        if (!protocols.includes('st3.bearer.paired-native-bearer')) { socket.end('HTTP/1.1 401 Unauthorized\r\n\r\n'); return; }
        const protocol = protocols[0];
        const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
        socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\nSec-WebSocket-Protocol: ${protocol}\r\n\r\n`);
        const message = protocol === 'st3.client.terminal.v0'
            ? require('../../../docs/st3/client-v0/fixtures/terminal-screen.json')
            : protocol === 'st3.client.conversation.v0'
                ? envelope({ kind: 'conversation-changes', session_id: 'session/example', items: [], next_cursor: 'cursor/next' })
                : { kind: 'snapshot', id: 'missions', collection: 'missions', snapshot, items: [], order: [], has_more: false };
        socket.write(frame(message));
        socket.on('data', () => socket.end());
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'st3-browser-'));
    const chrome = spawn(process.env.CHROME_BIN ?? 'chromium', ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--no-first-run', `--user-data-dir=${profile}`, `http://127.0.0.1:${server.address().port}`], { stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '';
    chrome.stderr.on('data', chunk => { stderr += chunk; });
    chrome.on('error', rejectResult);
    chrome.on('exit', code => rejectResult(new Error(`Chromium exited (${code}): ${stderr}`)));
    const timeout = setTimeout(() => rejectResult(new Error(`Browser test timed out: ${stderr}`)), 25000);
    try {
        assert.deepEqual(await result, { ok: true });
        assert.deepEqual(upgrades, [
            { url: '/v1/client/terminals/terminal%2Frelease-shell/stream?incarnation=pty-1', protocols: ['st3.client.terminal.v0', 'st3.cap.attach-proof', 'st3.bearer.paired-native-bearer'], authorization: undefined },
            { url: '/v1/client/conversations/example/stream?after=cursor%2Fexample', protocols: ['st3.client.conversation.v0', 'st3.bearer.paired-native-bearer'], authorization: undefined },
            { url: '/v1/client/collections/stream', protocols: ['st3.client.collections.v0', 'st3.bearer.paired-native-bearer'], authorization: undefined },
        ]);
    } finally {
        clearTimeout(timeout);
        const exited = new Promise(resolve => chrome.once('exit', resolve));
        chrome.kill();
        if (chrome.exitCode === null && chrome.signalCode === null) await exited;
        for (const socket of sockets) socket.destroy();
        await new Promise(resolve => server.close(resolve));
        fs.rmSync(profile, { recursive: true, force: true });
    }
});

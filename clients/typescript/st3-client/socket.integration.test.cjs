const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const { createHash } = require('node:crypto');
const { spawn } = require('node:child_process');
const ts = require('typescript');

// Exercise each runtime's actual WebSocket constructor, not a socket factory double.
test('native default sockets send Authorization without a bearer subprotocol', { timeout: 15000 }, async t => {
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'st3-native-sockets-'));
    const sockets = new Set();
    const upgrades = [];
    const server = http.createServer();
    server.on('upgrade', (request, socket) => {
        sockets.add(socket);
        socket.on('close', () => sockets.delete(socket));
        socket.on('error', () => {});
        const protocols = request.headers['sec-websocket-protocol'].split(',').map(value => value.trim());
        upgrades.push({ authorization: request.headers.authorization, protocols });
        const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
        socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\nSec-WebSocket-Protocol: ${protocols[0]}\r\n\r\n`);
        const kind = protocols[0] === 'st3.client.terminal.v0' ? 'terminal-screen' : 'conversation-changes';
        const message = Buffer.from(JSON.stringify({ api_version: 'st3.client.v0', value: { kind } }));
        socket.write(Buffer.concat([Buffer.from([0x81, message.length]), message]));
        socket.on('data', data => { if ((data[0] & 0x0f) === 8) socket.end(Buffer.from([0x88, 0])); });
    });
    try {
        for (const name of ['Models.generated', 'Client.generated']) {
            const source = fs.readFileSync(path.join(__dirname, `${name}.ts`), 'utf8');
            const output = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020, rewriteRelativeImportExtensions: true } }).outputText;
            fs.writeFileSync(path.join(temporary, `${name}.js`), output);
        }
        await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
        const baseUrl = `http://127.0.0.1:${server.address().port}`;
        const program = `
            const { St3Client } = require(${JSON.stringify(path.join(temporary, 'Client.generated.js'))});
            const client = new St3Client({ baseUrl: ${JSON.stringify(baseUrl)}, credential: () => 'native-header-proof' });
            (async () => {
                for (const kind of ['terminal', 'conversation', 'collection']) {
                    let received, failed;
                    const ready = new Promise((resolve, reject) => { received = resolve; failed = reject; });
                    const onEnd = error => failed(error ?? new Error('Closed before ready'));
                    const stream = kind === 'terminal'
                        ? await client.terminalStream('terminal/example', { streamCapability: 'attach-proof', onScreen: received, onEnd })
                        : kind === 'conversation'
                            ? await client.conversationStream('example', { onChange: received, onEnd })
                            : await client.collectionStream({ onOpen: received, onFrame() {}, onEnd });
                    await ready;
                    stream.close();
                }
            })().catch(error => { console.error(error); process.exit(1); });
        `;
        for (const [name, runtime] of [['Node', process.execPath], ['Bun', 'bun']]) {
            await t.test(name, async t => {
                const start = upgrades.length;
                const child = spawn(runtime, ['--eval', program], { stdio: ['ignore', 'pipe', 'pipe'] });
                let output = '';
                child.stdout.on('data', data => { output += data; });
                child.stderr.on('data', data => { output += data; });
                const deadline = setTimeout(() => child.kill('SIGKILL'), 5000);
                try {
                    const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('exit', resolve); });
                    assert.equal(code, 0, `${runtime}: ${output}`);
                } catch (error) {
                    if (name === 'Bun' && error.code === 'ENOENT') {
                        t.skip('Bun is not on PATH; the Bun handshake proof is local-only');
                        return;
                    }
                    throw error;
                } finally {
                    clearTimeout(deadline);
                    child.kill();
                }
                const expected = ['st3.client.terminal.v0', 'st3.client.conversation.v0', 'st3.client.collections.v0'];
                const received = upgrades.slice(start);
                assert.equal(received.length, 3);
                for (const [index, upgrade] of received.entries()) {
                    assert.equal(upgrade.authorization, 'Bearer native-header-proof');
                    assert.deepEqual(upgrade.protocols, [expected[index], ...(index === 0 ? ['st3.cap.attach-proof'] : [])]);
                }
            });
        }
    } finally {
        for (const socket of sockets) socket.destroy();
        await new Promise(resolve => server.close(resolve));
        fs.rmSync(temporary, { recursive: true, force: true });
    }
});

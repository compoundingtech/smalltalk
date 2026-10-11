import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import { createServer } from 'vite'

const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true } })
after(() => server.close())
const { errorReason, failedCommandSource, workLogTurnFromItems } = await server.ssrLoadModule('/src/assistant-ui/taste/work-log.ts')
const at = '2026-01-15T12:30:00Z'
const project = content => workLogTurnFromItems([{ _tag: 'ToolCall', id: 'tool/read', callId: 'call/read', name: 'read', input: {}, status: 'success', callSeen: true, at, result: { content, isError: false, at } }], { kindFor: () => 'read', running: false, failed: false, interrupted: false }).calls[0]

test('string output remains the work-log detail', () => assert.equal(project('Native output').detail, 'Native output'))
test('native content arrays retain every text part in order', () => assert.equal(project([{ type: 'text', text: 'First' }, { type: 'text', text: 'Second' }]).detail, 'First\nSecond'))
test('empty output has no detail', () => {
  for (const content of ['', [], undefined, [{ type: 'text', text: '' }]]) assert.equal(project(content).detail, undefined)
})
test('non-text parts are ignored, not serialized into invented output', () => {
  assert.equal(project([{ type: 'image', data: 'synthetic-bytes', mimeType: 'image/png' }, { type: 'text', text: 'Visible output' }, { type: 'audio', data: 'synthetic-audio' }]).detail, 'Visible output')
  assert.equal(project([{ type: 'image', data: 'synthetic-bytes' }]).detail, undefined)
})

const timeoutTrace = [
  'Traceback (most recent call last):',
  '  File "/usr/lib/python3.13/subprocess.py", line 571, in run',
  '    stdout, stderr = process.communicate(input, timeout=timeout)',
  '  File "/usr/lib/python3.13/subprocess.py", line 1251, in communicate',
  '    self._check_timeout(endtime, orig_timeout, stdout, stderr)',
  "subprocess.TimeoutExpired: Command 'sleep 60' timed out after 30 seconds",
].join('\n')

test('failed-command reason uses the exception, not traceback source', () => {
  assert.equal(errorReason(timeoutTrace), 'Command timed out after 30s')
})
test('a chained traceback uses the final named exception', () => {
  assert.equal(errorReason([
    timeoutTrace,
    'During handling of the above exception, another exception occurred:',
    'Traceback (most recent call last):',
    '  File "/app/parse.py", line 42, in load',
    '    return json.loads(output)',
    'json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)',
  ].join('\n')), 'json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)')
})
test('exception classes need not end in Error or Exception', () => {
  assert.equal(errorReason('Traceback (most recent call last):\n    raise StopIteration()\nStopIteration: no more rows'), 'StopIteration: no more rows')
  assert.equal(errorReason('JSONDecodeError: Expecting value'), 'JSONDecodeError: Expecting value')
})
test('exit code is the fallback when a traceback contains only frames and source', () => {
  const truncated = timeoutTrace.split('\n').slice(0, -1).join('\n')
  assert.equal(errorReason(`${truncated}\nProcess exited with code 2`), 'Exited with code 2')
  assert.equal(errorReason(`${truncated}\nExit code: 7`), 'Exited with code 7')
  assert.equal(errorReason(truncated), 'Tool failed; no readable reason was recorded.')
})
test('plain readable failures and path redaction retain their existing behavior', () => {
  assert.equal(errorReason('error: Cannot open /srv/app/project/source.ts'), 'error: Cannot open …/source.ts')
  assert.equal(errorReason('No matching files were found'), 'No matching files were found')
  assert.equal(errorReason(''), 'Tool failed; no readable reason was recorded.')
})

// A real failed cell (backup path neutralized): harness-truncated `sh -c` argv inside TimeoutExpired, followed by the harness exit code.
const realTimeout = [
  "Traceback (most recent call last):",
  "  File \"<cell>\", line 31, in <module>",
  "  File \"/nix/store/d64q19q1xjdwfhqx6czvrjgrhq0n3lcc-python3-3.14.7/lib/python3.14/subprocess.py\", line 557, in run",
  "    stdout, stderr = process.communicate(input, timeout=timeout)",
  "                     ~~~~~~~~~~~~~~~~~~~^^^^^^^^^^^^^^^^^^^^^^^^",
  "  File \"/nix/store/d64q19q1xjdwfhqx6czvrjgrhq0n3lcc-python3-3.14.7/lib/python3.14/subprocess.py\", line 1221, in communicate",
  "    stdout, stderr = self._communicate(input, endtime, timeout)",
  "                     ~~~~~~~~~~~~~~~~~^^^^^^^^^^^^^^^^^^^^^^^^^",
  "  File \"/nix/store/d64q19q1xjdwfhqx6czvrjgrhq0n3lcc-python3-3.14.7/lib/python3.14/subprocess.py\", line 2154, in _communicate",
  "    self._check_timeout(endtime, orig_timeout, stdout, stderr)",
  "    ~~~~~~~~~~~~~~~~~~~^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^",
  "  File \"/nix/store/d64q19q1xjdwfhqx6czvrjgrhq0n3lcc-python3-3.14.7/lib/python3.14/subprocess.py\", line 1268, in _check_timeout",
  "    raise TimeoutExpired(",
  "    ...<2 lines>...",
  "            stderr=b''.join(stderr_seq) if stderr_seq else None)",
  "subprocess.TimeoutExpired: Command '['sh', '-c', '\\nset -eu\\nmkdir -p \"$HOME/.omp/agent/agents\"\\necho \\'tools: {artifactSpillThreshold: 16}\\' > \"$HOME/.omp/agent/config.yml\"; chmod 600 \"$HOME/.omp/agent/config.yml\"\\necho a > \"$HOME/.omp/agent/agents/scout.md\"\\nTS=$(date -u +%Y%m%dT%H%M%SZ)\\nB=\"$HOME/.cache/example-backup/$TS\"\\numask 077\\nmkdir -p \"$B\"\\ncp -p \"$HOME/.omp/agent/config.yml\" \"$B/config.yml\"\\nif [ -d \"$HOME/.omp/agent/agents\" ]; then\\n  mv \"$HOME/.omp/agent/agents\" \"$B/agents\"\\nfi\\n( cd \"$B\" && sha256sum config.yml > SHA256SUMS && if [ -d agents ]; then find agents -type f -exec sha256sum {} + >> SHA256SUMS; fi )\\necho \"B=$B\"\\ntest ! -e \"$HOME/.omp/agent/agents\" && echo agents-aside-ok\\nstat -c \\'%a %n\\' \"$B\" \"$B/config.yml\"\\n( cd\u2026",
  "",
  "Command exited with code 1",
  "",
  "[Some lines truncated to 768 bytes. Read artifact://example for full output]",
].join('\n')

test('subprocess exceptions summarize the outcome and never print argv or script source', () => {
  assert.equal(errorReason(realTimeout), 'Command timed out')
  assert.equal(errorReason("subprocess.CalledProcessError: Command '['git', 'push']' returned non-zero exit status 128."), 'Command exited with code 128')
  assert.equal(errorReason("CalledProcessError: Command '['sh', '-c', 'make\\n']' died with <Signals.SIGKILL: 9>.\nExit code: 137"), 'Command exited with code 137')
})
test('the failed command is recovered unescaped for the disclosure', () => {
  const script = failedCommandSource(realTimeout)
  assert.match(script, /^set -eu\nmkdir -p "\$HOME\/\.omp\/agent\/agents"\necho 'tools: \{artifactSpillThreshold: 16\}' > /)
  assert.ok(!script.includes('\\n'))
  assert.equal(failedCommandSource(timeoutTrace), 'sleep 60')
  assert.equal(failedCommandSource("subprocess.CalledProcessError: Command '['git', 'push', 'origin']' returned non-zero exit status 1."), 'git push origin')
  assert.equal(failedCommandSource('error: plain failure'), undefined)
})
test('first-level reasons are capped to one short line', () => {
  const reason = errorReason(`ValueError: ${'x'.repeat(400)}`)
  assert.equal(reason.length, 160)
  assert.ok(reason.endsWith('…') && !reason.includes('\n'))
})

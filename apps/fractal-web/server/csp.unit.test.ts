import { createHash } from 'node:crypto'
import { expect, it } from 'vitest'
import { createContentSecurityPolicy } from './csp.mts'

it('hashes only inline scripts, normalizing HTML line endings and deduplicating hashes', () => {
  const code = '\nwindow.example = true\n'
  const hash = createHash('sha256').update(code).digest('base64')
  const policy = createContentSecurityPolicy(`<script>${code}</script><script type="module">${code.replaceAll('\n', '\r\n')}</script><script src="/bundle.js"></script>`)
  expect(policy('app.example')).toContain(`script-src 'self' 'sha256-${hash}';`)
  expect(policy('app.example')).not.toContain("'unsafe-eval'")
  expect(policy('app.example')).toContain("connect-src 'self' ws://app.example wss://app.example;")
})
it('allows only explicitly configured connection origins, never arbitrary CSP source expressions', () => {
  const policy = createContentSecurityPolicy('', ['https://collector.example', 'https://collector.example'])
  expect(policy('app.example')).toContain("connect-src 'self' ws://app.example wss://app.example https://collector.example;")
  for (const origin of ['*', 'https://*.example', 'data:', 'https://user:' + 'pass@example', 'https://example/path', 'https://example;img-src', "https://example;img-src *"]) {
    expect(() => createContentSecurityPolicy('', [origin])).toThrow(TypeError)
  }
})
it('does not interpret a malformed request authority as additional source expressions', () => {
  const policy = createContentSecurityPolicy()
  expect(policy('app.example')).toContain("img-src 'self' data: blob:")
  expect(() => policy('app.example;img-src *')).toThrow(TypeError)
  expect(() => policy('user@app.example')).toThrow(TypeError)
  expect(policy('[::1]:8080')).toContain('ws://[::1]:8080 wss://[::1]:8080')
})
it('permits Vite reconnect blob workers only in development', () => {
  expect(createContentSecurityPolicy()('app.example')).not.toContain('worker-src')
  expect(createContentSecurityPolicy('', [], { development: true })('app.example')).toContain("worker-src 'self' blob:")
})

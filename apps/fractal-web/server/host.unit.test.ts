import type { IncomingMessage } from 'node:http'

import { describe, expect, it } from 'vitest'

import { readHostConfig, tailnetAdmission } from './host.mts'

const origin = 'https://atlas.example.ts.net:8443'
const request = (headers: IncomingMessage['headers'], method = 'GET') => ({ headers, method }) as IncomingMessage
const admit = tailnetAdmission({ origin, logins: ['river@example.com'], listener: () => '127.0.0.1:7410' })
const signedIn = { 'tailscale-user-login': 'river@example.com' }

describe('tailnetAdmission', () => {
  it('admits the allowed login on the published origin, for reads, sends and upgrades', () => {
    expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443' }))).toBe(true)
    expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', origin }, 'POST'))).toBe(true)
    expect(admit(request({ ...signedIn, host: '127.0.0.1:7410', origin, upgrade: 'websocket' }))).toBe(true)
  })

  it('refuses a missing or other login', () => {
    expect(admit(request({ host: 'atlas.example.ts.net:8443', origin }))).toBe(false)
    expect(admit(request({ 'tailscale-user-login': 'cedar@example.com', host: 'atlas.example.ts.net:8443', origin }))).toBe(false)
  })

  it('refuses a login header that carries more than one value', () => {
    // Node joins a repeated header into one comma-separated value, so a forged copy next to the real one never matches.
    for (const joined of ['river@example.com, cedar@example.com', 'cedar@example.com, river@example.com', 'river@example.com,river@example.com'])
      expect(admit(request({ 'tailscale-user-login': joined, host: 'atlas.example.ts.net:8443', origin }))).toBe(false)
  })

  it('refuses foreign origins and hosts', () => {
    for (const other of ['https://example.com', 'null', 'http://atlas.example.ts.net:8443', 'https://atlas.example.ts.net']) {
      expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', origin: other }))).toBe(false)
      expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', origin: other, upgrade: 'websocket' }))).toBe(false)
    }
    expect(admit(request({ ...signedIn, host: 'rebind.example:8443', origin }))).toBe(false)
    expect(admit(request({ ...signedIn, origin }))).toBe(false)
  })

  it('admits Origin-less reads only, and not when the browser says they came from another site', () => {
    expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443' }, 'HEAD'))).toBe(true)
    for (const site of ['same-origin', 'none']) expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', 'sec-fetch-site': site }))).toBe(true)
    for (const site of ['cross-site', 'same-site']) expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', 'sec-fetch-site': site }))).toBe(false)
    expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443' }, 'POST'))).toBe(false)
    expect(admit(request({ ...signedIn, host: 'atlas.example.ts.net:8443', upgrade: 'websocket' }))).toBe(false)
  })

  it('requires an exact https origin and at least one login', () => {
    expect(() => tailnetAdmission({ origin: 'http://atlas.example.ts.net', logins: ['river@example.com'], listener: () => undefined })).toThrow()
    expect(() => tailnetAdmission({ origin: `${origin}/`, logins: ['river@example.com'], listener: () => undefined })).toThrow()
    expect(() => tailnetAdmission({ origin, logins: [], listener: () => undefined })).toThrow()
  })
})

describe('readHostConfig', () => {
  const env = {
    WF_PORT: '7410', WF_PUBLIC_ORIGIN: origin, WF_TAILNET_LOGINS: 'river@example.com, cedar@example.com',
    WF_ST_GATEWAY: 'unix:/run/example/st3-client.sock', WF_ST_AUTHORIZATION_FILE: '/secrets/web-bearer', WF_DIST: '/srv/web',
  }
  const secret = (text: string, mode = 0o100600, uid = 1000) => ({
    readText: (path: string) => { if (path === '/secrets/web-bearer') return text; throw new Error(path) },
    owner: () => ({ mode, uid }),
  })
  const files = secret('synthetic-token\n')

  it('reads the bearer from its file and defaults to a loopback listener', () => {
    expect(readHostConfig(env, files, 1000)).toEqual({
      dist: '/srv/web', host: '127.0.0.1', port: 7410, origin, logins: ['river@example.com', 'cedar@example.com'],
      socketPath: '/run/example/st3-client.sock', authorization: ['Bearer', 'synthetic-token'].join(' '), deploymentId: 'atlas.example.ts.net:8443',
    })
  })

  it('refuses a non-loopback listener, a missing setting, no logins and a malformed bearer', () => {
    expect(() => readHostConfig({ ...env, WF_HOST: '0.0.0.0' }, files, 1000)).toThrow(/loopback/)
    expect(() => readHostConfig({ ...env, WF_PORT: '' }, files, 1000)).toThrow(/WF_PORT/)
    expect(() => readHostConfig({ ...env, WF_TAILNET_LOGINS: ' , ' }, files, 1000)).toThrow(/WF_TAILNET_LOGINS/)
    expect(() => readHostConfig(env, secret('two tokens'), 1000)).toThrow(/one bearer/)
  })

  it('refuses a bearer file that others can read or another user owns', () => {
    expect(() => readHostConfig(env, secret('synthetic-token', 0o100640), 1000)).toThrow(/chmod 600/)
    expect(() => readHostConfig(env, secret('synthetic-token', 0o100604), 1000)).toThrow(/chmod 600/)
    expect(() => readHostConfig(env, secret('synthetic-token', 0o100600, 0), 1000)).toThrow(/chmod 600/)
  })
})

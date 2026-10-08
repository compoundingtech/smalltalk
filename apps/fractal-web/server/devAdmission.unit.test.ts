import type { IncomingMessage } from 'node:http'

import { describe, expect, it } from 'vitest'

import { devAdmission } from './devAdmission.mts'

const request = (headers: IncomingMessage['headers']) => ({ headers }) as IncomingMessage
const admit = devAdmission(() => 5173)

describe('devAdmission', () => {
  it('admits the Vite origin for HTTP and WebSocket upgrades', () => {
    expect(admit(request({ host: '127.0.0.1:5173', origin: 'http://127.0.0.1:5173' }))).toBe(true)
    expect(admit(request({ host: 'localhost:5173', origin: 'http://localhost:5173', upgrade: 'websocket' }))).toBe(true)
  })

  it('admits Origin-less HTTP but not Origin-less upgrades', () => {
    expect(admit(request({ host: '127.0.0.1:5173' }))).toBe(true)
    expect(admit(request({ host: '127.0.0.1:5173', upgrade: 'websocket' }))).toBe(false)
  })

  it('rejects foreign, null and other-port loopback origins', () => {
    for (const origin of ['https://example.com', 'null', 'http://127.0.0.1:8080', 'https://127.0.0.1:5173']) {
      expect(admit(request({ host: '127.0.0.1:5173', origin, upgrade: 'websocket' }))).toBe(false)
      expect(admit(request({ host: '127.0.0.1:5173', origin }))).toBe(false)
    }
  })

  it('rejects non-loopback Host headers and an unbound listener', () => {
    expect(admit(request({ host: 'rebind.example:5173', origin: 'http://127.0.0.1:5173' }))).toBe(false)
    expect(devAdmission(() => undefined)(request({ host: '127.0.0.1:5173' }))).toBe(false)
  })
})

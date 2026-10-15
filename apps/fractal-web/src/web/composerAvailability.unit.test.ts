import { expect, it } from 'vitest'
import { composerAvailability } from './composerAvailability.ts'

it('distinguishes missing grant, offline and native terminal authorization failure', () => {
  expect(composerAvailability({ online: true, readable: true, granted: 'ungranted' })._tag).toBe('MissingGrant')
  expect(composerAvailability({ online: false, readable: true, granted: 'granted' })._tag).toBe('Offline')
  expect(composerAvailability({ online: true, readable: true, granted: 'granted', refusal: { reason: 'ungranted', status: 401 } })._tag).toBe('Revoked')
  expect(composerAvailability({ online: false, readable: true, granted: 'ungranted', refusal: { reason: 'ungranted', status: 403 } })._tag).toBe('Revoked')
})
it('never treats retained outbox content as authorization to read a refused conversation', () => {
  expect(composerAvailability({ online: true, readable: false, granted: 'granted' })._tag).toBe('Unavailable')
  expect(composerAvailability({ online: true, readable: true, granted: 'granted' })._tag).toBe('Ready')
})

import { describe, expect, it } from 'vitest'
import { installPageLifecycle } from './pageLifecycle.ts'

describe('document-owned synchronous socket teardown', () => {
  it('closes before starting asynchronous scope disposal on navigation', () => {
    const target = new EventTarget(), calls: string[] = []
    const uninstall = installPageLifecycle({ target, beforeDispose: () => { calls.push('unmount') }, source: {
      suspendSockets: () => { calls.push('close-1001') },
      resumeSockets: () => { calls.push('resume') },
      dispose: async () => { calls.push('dispose') },
    } })
    target.dispatchEvent(Object.assign(new Event('pagehide'), { persisted: false }))
    expect(calls).toEqual(['close-1001', 'unmount', 'dispose'])
    target.dispatchEvent(Object.assign(new Event('pageshow'), { persisted: false }))
    expect(calls).toEqual(['close-1001', 'unmount', 'dispose'])
    uninstall()
  })
  it('retains the scope for bfcache, resumes on persisted pageshow, and removes listeners', () => {
    const target = new EventTarget(), calls: string[] = []
    const uninstall = installPageLifecycle({ target, beforeDispose: () => { calls.push('unmount') }, source: {
      suspendSockets: () => { calls.push('close-1001') },
      resumeSockets: () => { calls.push('resume') },
      dispose: async () => { calls.push('dispose') },
    } })
    target.dispatchEvent(Object.assign(new Event('pagehide'), { persisted: true }))
    expect(calls).toEqual(['close-1001'])
    target.dispatchEvent(Object.assign(new Event('pageshow'), { persisted: true }))
    expect(calls).toEqual(['close-1001', 'resume'])
    uninstall()
    target.dispatchEvent(Object.assign(new Event('pagehide'), { persisted: false }))
    expect(calls).toEqual(['close-1001', 'resume'])
  })
})

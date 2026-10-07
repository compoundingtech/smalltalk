import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createExtensions } from './public.ts'
import { createExtensionRegistry } from './registry.ts'

test('the default composition is empty and immutable', () => {
  const empty = createExtensions<string, string, string>()
  assert.deepEqual(empty, { panes: [], hosts: [], claims: [] })
  assert.ok(Object.isFrozen(empty))
  assert.ok(Object.values(empty).every(Object.isFrozen))
})

test('public and extension claims remain separate; combined claims use the host validator', () => {
  const publicClaims = ['agent/detail']
  const panes = ['sample-pane']
  const hosts = ['sample-host']
  const extensionClaims = ['agent/overview']
  let validated: readonly string[] | undefined
  const registry = createExtensionRegistry({ publicClaims,
    extensions: { panes, hosts, claims: extensionClaims },
    validateClaims: (claims) => { validated = claims },
  })
  assert.equal(validated, registry.claims)
  publicClaims.push('late/public')
  extensionClaims.push('late/extension')
  panes.push('late/pane')
  hosts.push('late/host')
  assert.deepEqual(registry.publicClaims, ['agent/detail'])
  assert.deepEqual(registry.extensionClaims, ['agent/overview'])
  assert.deepEqual(registry.claims, ['agent/detail', 'agent/overview'])
  assert.deepEqual(registry.panes, ['sample-pane'])
  assert.deepEqual(registry.hosts, ['sample-host'])
  assert.ok(Object.isFrozen(registry))
})

test('a conflicting extension cannot override a public claim', () => {
  assert.throws(() => createExtensionRegistry({
    publicClaims: ['agent/detail'], extensions: { panes: [], hosts: [], claims: ['agent/detail'] },
    validateClaims: (claims) => {
      if (new Set(claims).size !== claims.length) throw new Error('Claim conflict')
    },
  }), /Claim conflict/)
})

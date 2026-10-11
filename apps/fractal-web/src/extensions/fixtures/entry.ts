import { createExtensions } from '../build.ts'
import { createExtensionRegistry } from '../registry.ts'

export const registry = createExtensionRegistry({
  publicClaims: ['sample/detail'],
  extensions: createExtensions<string, string, string>(),
  validateClaims: (claims) => {
    if (new Set(claims).size !== claims.length) throw new Error('Claim conflict')
  },
})

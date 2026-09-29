// The app's tests run its TypeScript modules directly in Node. Metro and tsc resolve extensionless
// and directory imports; this hook resolves them the same way for Node.
import { registerHooks } from 'node:module';

registerHooks({
  resolve(specifier, context, nextResolve) {
    try { return nextResolve(specifier, context); } catch (error) {
      if (!specifier.startsWith('.') || (error.code !== 'ERR_MODULE_NOT_FOUND' && error.code !== 'ERR_UNSUPPORTED_DIR_IMPORT')) throw error;
      for (const suffix of ['.ts', '.tsx', '/index.ts']) {
        try { return nextResolve(specifier + suffix, context); } catch { /* try the next form */ }
      }
      throw error;
    }
  },
});

import { defineConfig } from 'vite'
import { execFileSync } from 'node:child_process'
import react from '@vitejs/plugin-react'
import { createStylexVitePlugins } from './scripts/stylex.mjs'
import { webfractalGateway } from './scripts/vite-gateway.mjs'
import { extensionBuild } from './scripts/extensions-build.mjs'
import { fractalContentSecurityPolicy } from './scripts/vite-csp.mjs'
export default defineConfig({
  root: 'src/web',
  plugins: [
    fractalContentSecurityPolicy(),
    {
      name: 'fractal-build-identity',
      resolveId(id) {
        if (id === 'virtual:build-identity') return '\0fractal-build-identity'
      },
      load(id) {
        if (id !== '\0fractal-build-identity') return
        const rev = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
        return (
          'export const buildIdentity=' +
          JSON.stringify({ baseVersion: '0.1.0', machineVersion: '0.1.0+' + rev.slice(0, 12), gitRev: rev }) +
          ';export const deploymentId=undefined;'
        )
      },
    },
    extensionBuild({ entry: '../src/extensions/app-public.ts', privateBuild: false }),
    ...createStylexVitePlugins({
      externalPackages: ['@smalltalk/fractal-ui', '@overeng/devbar', '@overeng/meters', '@overeng/stylex-tokens'],
      entries: [new URL('./src/web/main.tsx', import.meta.url).pathname],
    }),
    react(),
    webfractalGateway(),
  ],
  server: { strictPort: true, host: '127.0.0.1', allowedHosts: (process.env.WF_ALLOWED_HOSTS ?? '127.0.0.1,localhost').split(',') },
  resolve: { dedupe: ['effect', 'react', 'react-dom', '@stylexjs/stylex'] },
  build: {
    outDir: '../../dist/web',
    emptyOutDir: true,
    rolldownOptions: {
      preserveEntrySignatures: false,
      output: {
        strictExecutionOrder: true,
        codeSplitting: {
          // Capture only matched modules: pulling their dependencies into a shared group
          // can promote conversation-only code into the shell's eager import graph.
          includeDependenciesRecursively: false,
          groups: [
            { name: 'react', test: /node_modules[\\/](?:react|react-dom|scheduler)[\\/]/, entriesAware: true },
            { name: 'effect', test: /node_modules[\\/](?:effect|@effect[\\/]atom-react)[\\/]/, entriesAware: true },
            { name: 'aria', test: /node_modules[\\/](?:react-aria|react-aria-components|react-stately|@react-aria|@react-stately|@internationalized)[\\/]/, entriesAware: true },
          ],
        },
      },
    },
  },
})

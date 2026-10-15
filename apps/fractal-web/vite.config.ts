import { defineConfig } from 'vite'
import { execFileSync } from 'node:child_process'
import react from '@vitejs/plugin-react'
import { createStylexVitePlugins, deferBuiltStylesheets } from './scripts/stylex.mjs'
import { webfractalGateway } from './scripts/vite-gateway.mjs'
import { extensionBuild } from './scripts/extensions-build.mjs'
import { fractalContentSecurityPolicy } from './scripts/vite-csp.mjs'
import { fractalCssAssets } from './scripts/vite-css-assets.mjs'

// One identity per build: the bundle embeds it and the host serves it as /build-identity.json.
const git = (...args: string[]) => execFileSync('git', args, { encoding: 'utf8' }).trim()
const buildIdentity = (() => {
  const rev = git('rev-parse', 'HEAD')
  const dirty = git('status', '--porcelain', '--untracked-files=no') !== ''
  const machineVersion = '0.1.0+' + rev.slice(0, 12)
  return {
    baseVersion: '0.1.0', displayVersion: dirty ? machineVersion + '-dirty' : machineVersion, machineVersion,
    sourceKind: 'local', rev, gitRev: rev, dirty, commitTs: Number(git('show', '-s', '--format=%ct', 'HEAD')), buildTs: Math.floor(Date.now() / 1000),
  } as const
})()
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
        return 'export const buildIdentity=' + JSON.stringify(buildIdentity) + ';export const deploymentId=undefined;'
      },
      generateBundle() {
        // Read by server/host.mts; the server answers /build-identity.json itself, so this file is never served.
        const { gitRev: _, ...served } = buildIdentity
        this.emitFile({ type: 'asset', fileName: 'build-identity.json', source: JSON.stringify(served) + '\n' })
      },
    },
    extensionBuild({ entry: '../src/extensions/app-public.ts', privateBuild: false }),
    ...createStylexVitePlugins({
      externalPackages: ['@smalltalk/fractal-ui'],
      entries: [new URL('./src/web/main.tsx', import.meta.url).pathname],
    }),
    deferBuiltStylesheets(),
    react(),
    webfractalGateway(),
    fractalCssAssets(),
  ],
  server: { strictPort: true, host: '127.0.0.1', allowedHosts: (process.env.WF_ALLOWED_HOSTS ?? '127.0.0.1,localhost').split(',') },
  resolve: { dedupe: ['effect', 'react', 'react-dom'] },
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

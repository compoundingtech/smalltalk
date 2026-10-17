import { defineConfig } from 'vite'
import { createBuildIdentityPlugin } from '@overeng/utils/node/vite-build-identity'
import react from '@vitejs/plugin-react'
import { createStylexVitePlugins } from './scripts/stylex.mjs'
import { webfractalGateway } from './scripts/vite-gateway.mjs'
import { extensionBuild } from './scripts/extensions-build.mjs'
import { fractalContentSecurityPolicy } from './scripts/vite-csp.mjs'
import { fractalCssAssets } from './scripts/vite-css-assets.mjs'

export default defineConfig({
  root: 'src/web',
  plugins: [
    fractalContentSecurityPolicy(),
    createBuildIdentityPlugin({ baseVersion: '0.1.0', buildStamp: '__CLI_BUILD_STAMP__' }),
    extensionBuild({ entry: '../src/extensions/app-public.ts', privateBuild: false }),
    ...createStylexVitePlugins({
      externalPackages: ['@smalltalk/fractal-ui', '@overeng/devbar', '@overeng/meters', '@overeng/stylex-tokens'],
      entries: [new URL('./src/web/main.tsx', import.meta.url).pathname],
    }),
    react(),
    webfractalGateway(),
    fractalCssAssets(),
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

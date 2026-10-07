import { defineConfig } from 'vite'
import { execFileSync } from 'node:child_process'
import react from '@vitejs/plugin-react'
import { createStylexVitePlugins } from './scripts/stylex.mjs'
import { webfractalGateway } from './scripts/vite-gateway.mjs'
import { extensionBuild } from './scripts/extensions-build.mjs'
export default defineConfig({
  root: 'src/web',
  plugins: [
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
      externalPackages: ['@smalltalk/fractal-ui'],
      entries: [new URL('./src/web/main.tsx', import.meta.url).pathname],
    }),
    react(),
    webfractalGateway(),
  ],
  server: { strictPort: true, host: '127.0.0.1', allowedHosts: (process.env.WF_ALLOWED_HOSTS ?? '127.0.0.1,localhost').split(',') },
  resolve: { dedupe: ['effect', 'react', 'react-dom'] },
  build: { outDir: '../../dist/web', emptyOutDir: true },
})

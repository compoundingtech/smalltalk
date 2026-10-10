import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { composition } from './composition.mjs'
import type { StorybookConfig } from '@storybook/react-vite'
import tailwindcss from '@tailwindcss/vite'
import stylex from '@stylexjs/unplugin'

const config: StorybookConfig = {
  ...composition({
    repoRoot: fileURLToPath(new URL('../../../', import.meta.url)),
    appUrl: process.env.FRACTAL_APP_STORYBOOK_URL,
    build: process.argv.includes('build'),
    ci: Boolean(process.env.CI),
  }),
  env: (env) => ({ ...env, STORYBOOK_FRACTAL_REVISION: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: fileURLToPath(new URL('../../../', import.meta.url)), encoding: 'utf8' }).trim() }),
  stories: ['../src/**/*.stories.tsx'],
  framework: '@storybook/react-vite',
  addons: ['@storybook/addon-docs', '@storybook/addon-a11y'],
  core: { disableTelemetry: true },
  viteFinal: async (config) => ({ ...config, plugins: [...(config.plugins ?? []), tailwindcss(), stylex.vite({ useCSSLayers: false })] }),
}
export default config

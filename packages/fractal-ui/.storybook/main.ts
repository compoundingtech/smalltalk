import type { StorybookConfig } from '@storybook/react-vite'
import tailwindcss from '@tailwindcss/vite'
import { createStylexVitePlugins } from '../../../apps/fractal-web/scripts/stylex.mjs'

const config: StorybookConfig = {
  stories: [
    '../src/**/*.stories.tsx',
    '../../../apps/fractal-web/src/web/NarrowLayout.stories.tsx',
  ],
  staticDirs: ['./fixtures'],
  framework: '@storybook/react-vite',
  addons: ['@storybook/addon-docs', '@storybook/addon-a11y'],
  core: { disableTelemetry: true },
  viteFinal: async (config) => ({
    ...config,
    plugins: [
      ...(config.plugins ?? []),
      tailwindcss(),
      // Keep compiled rules on the eager preview entry, not a lazy app pane's CSS asset.
      ...createStylexVitePlugins({ entries: [new URL('./preview.ts', import.meta.url).pathname] }),
    ],
  }),
}
export default config

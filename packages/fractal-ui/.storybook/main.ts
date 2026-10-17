import type { StorybookConfig } from '@storybook/react-vite'
import tailwindcss from '@tailwindcss/vite'
import stylex from '@stylexjs/unplugin'

const config: StorybookConfig = {
  stories: ['../src/**/*.stories.tsx', '../../../apps/fractal-web/src/web/ColdOpen.stories.tsx'],
  staticDirs: ['./fixtures'],
  framework: '@storybook/react-vite',
  addons: ['@storybook/addon-docs', '@storybook/addon-a11y'],
  core: { disableTelemetry: true },
  viteFinal: async (config) => ({ ...config, plugins: [...(config.plugins ?? []), tailwindcss(), stylex.vite({ useCSSLayers: false })] }),
}
export default config

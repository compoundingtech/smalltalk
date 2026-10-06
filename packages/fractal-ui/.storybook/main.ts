import type { StorybookConfig } from '@storybook/react-vite'
import tailwindcss from '@tailwindcss/vite'

// Local workshop: helper reuse awaits contributor permission; no private producer config.
const config: StorybookConfig = {
  stories: ['../src/**/*.stories.tsx'],
  framework: '@storybook/react-vite',
  addons: ['@storybook/addon-docs', '@storybook/addon-a11y'],
  core: { disableTelemetry: true },
  viteFinal: async (config) => ({ ...config, plugins: [...(config.plugins ?? []), tailwindcss()] }),
}
export default config

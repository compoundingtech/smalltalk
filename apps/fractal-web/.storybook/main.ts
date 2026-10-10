import type { StorybookConfig } from '@storybook/react-vite'

// This workspace has no @overeng/utils-storybook helper. The builder loads the
// production config, including its StyleX compiler and virtual CSS pipeline.
const config: StorybookConfig = {
  stories: ['../src/web/{LiveAgentWorkspace,ConversationPane}.stories.tsx'],
  framework: {
    name: '@storybook/react-vite',
    options: { builder: { viteConfigPath: new URL('../vite.config.ts', import.meta.url).pathname } },
  },
  addons: ['@storybook/addon-docs', '@storybook/addon-a11y'],
  core: { disableTelemetry: true },
}
export default config

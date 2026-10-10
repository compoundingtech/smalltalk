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
  viteFinal: async config => ({
    ...config,
    // Storybook owns its manager/iframe HTML and middleware-mode HMR server
    // (builder-vite sets hmr.port and hmr.server). The app CSP hashes the app's
    // own index.html and forbids explicit HMR authorities; it is not a policy
    // for Storybook documents. Leave the unchanged production Vite config and
    // every other plugin intact, including the real StyleX pipeline.
    plugins: [
      ...(config.plugins?.filter(plugin =>
        !(plugin && typeof plugin === 'object' && 'name' in plugin && plugin.name === 'fractal-content-security-policy')) ?? []),
      {
        name: 'fractal-storybook-stylex-entry',
        apply: 'build',
        enforce: 'pre',
        // Production injects the collected CSS at main.tsx. The book's entry
        // is preview.ts instead; dev uses the same compiler's runtime injection.
        transform: (code, id) => id.split('?')[0] === new URL('./preview.ts', import.meta.url).pathname
          ? `${code}\nimport 'virtual:overeng-stylex.css'\n` : undefined,
      },
    ],
  }),
}
export default config

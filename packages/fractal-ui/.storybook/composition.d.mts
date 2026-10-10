import type { StorybookConfig } from '@storybook/react-vite'

export const appRef: { id: 'fractal-app'; title: 'Fractal App'; staticPath: string }
export function composition(options: { repoRoot: string; appUrl?: string; build: boolean; ci: boolean }): Pick<StorybookConfig, 'refs' | 'staticDirs'>

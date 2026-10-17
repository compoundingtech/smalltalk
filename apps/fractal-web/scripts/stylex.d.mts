import type { HtmlTagDescriptor, Plugin } from 'vite'

export const stylexVirtualCssId: string
export const deferStylexDevTags: (tags: readonly HtmlTagDescriptor[] | null | undefined) => readonly HtmlTagDescriptor[] | null | undefined
export const deferBuiltStylesheets: () => Plugin
export const createStylexVitePlugins: (options?: {
  readonly externalPackages?: readonly string[]
  readonly entries?: readonly string[]
  readonly useCSSLayers?: boolean | { readonly before?: readonly string[]; readonly after?: readonly string[]; readonly prefix?: string }
}) => Plugin[]

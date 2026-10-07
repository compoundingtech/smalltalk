import { assertBundleBoundary } from '../src/extensions/bundle-policy.mjs'

/** A fixed file replacement; there is no runtime loader, remote URL or environment-selected code. */
export const extensionBuild = ({ entry, privateBuild }) => ({
  name: 'fractal:build-time-extensions',
  enforce: 'pre',
  resolveId(source, importer) {
    if (source.endsWith('/extensions/build.ts') ||
      (source === './build.ts' && importer?.includes('/src/extensions/')))
      return new URL(entry, import.meta.url).pathname
  },
  generateBundle(_options, bundle) {
    const modules = [...this.getModuleIds()]
    const texts = Object.values(bundle).map((item) =>
      item.type === 'chunk' ? item.code : typeof item.source === 'string'
        ? item.source : new TextDecoder().decode(item.source))
    assertBundleBoundary({ modules, texts, privateBuild })
  },
})

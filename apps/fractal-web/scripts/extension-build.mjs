import { dirname, resolve } from 'node:path'

/** The resolver selects a local source file before bundling, never a runtime code URL. */
export const extensionBuildPlugin = ({ entry, composition }) => {
  const target = resolve(entry)
  const replacement = resolve(composition)
  return {
    name: 'fractal:extension-composition',
    enforce: 'pre',
    resolveId(source, importer) {
      if (source === target || (importer !== undefined && source.startsWith('.') &&
        resolve(dirname(importer), source) === target)) return replacement
    },
  }
}

/** Check every emitted chunk and asset, including source maps and lazy chunks. */
export const assertExtensionBundle = ({ bundle, forbiddenModules, forbiddenContent, requiredModules = [], requiredContent = [] }) => {
  const outputs = Object.values(bundle)
  const modules = outputs.flatMap((item) => item.type === 'chunk' ? Object.keys(item.modules) : [])
  const content = outputs.map((item) => item.type === 'chunk' ? item.code :
    typeof item.source === 'string' ? item.source : new TextDecoder().decode(item.source)).join('\n')
  for (const forbidden of forbiddenModules)
    if (modules.some((id) => forbidden.test(id))) throw new Error('Excluded extension module entered the bundle')
  for (const forbidden of forbiddenContent)
    if (content.includes(forbidden)) throw new Error('Excluded extension content entered the bundle')
  for (const required of requiredModules)
    if (!modules.some((id) => required.test(id))) throw new Error('Required extension module is absent')
  for (const required of requiredContent)
    if (!content.includes(required)) throw new Error('Required extension content is absent')
}

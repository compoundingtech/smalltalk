import { existsSync, readdirSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { compileChangelog, compileChangelogEntries } from '../src/changelog/compiler.ts'
import { changelogAudienceByDeployment } from '../src/changelog/contract.ts'
import { changelogRegistryDirectory, changelogRegistryModules, writeChangelogRegistry } from './changelog-registry.mjs'

// Native loading survives Vite's runner disposing the config evaluator before build hooks.
const nativeImport = createRequire(import.meta.url)('./changelog-native-import.cjs')

/** Fixed registry locations; the private one is absent from public checkouts. */
export const defaultChangelogRegistries = Object.freeze({
  public: fileURLToPath(new URL('../.changelog/mod.ts', import.meta.url)),
  private: fileURLToPath(new URL('../private/.changelog/mod.ts', import.meta.url)),
})

const leak = 'Public build leaked private changelog'

/** Every file beside the registry modules is an entry; the compiler requires a registry bijection with them. */
const listEntries = (registry, visibility) =>
  readdirSync(dirname(registry)).filter((file) => !changelogRegistryModules.includes(file)).map((file) => ({ file, visibility }))

/** The public registry is derived from its directory; derive it before anything imports it. */
export const ensureChangelogRegistries = (registries = defaultChangelogRegistries) => {
  if (dirname(registries.public) === changelogRegistryDirectory) writeChangelogRegistry()
}

const importRegistry = async (registry) => {
  const module = await nativeImport(pathToFileURL(registry).href)
  if (!Array.isArray(module.changelogSources) || typeof module.changelogDeployment !== 'string' || !Object.hasOwn(changelogAudienceByDeployment, module.changelogDeployment))
    throw new Error(`Changelog registry must export changelogSources and changelogDeployment: ${registry}`)
  return module
}

/**
 * Compiles the build's audience from source files before bundling. Public builds also compile the
 * private registry (when present) only to learn what must never appear in their output.
 */
export const prepareChangelogBuild = async ({ privateBuild, deployment, registries = defaultChangelogRegistries }) => {
  ensureChangelogRegistries(registries)
  const publicInventory = listEntries(registries.public, 'public')
  const hasPrivate = existsSync(registries.private)
  if (privateBuild && !hasPrivate) throw new Error(`Private build requires ${registries.private}`)
  const privateInventory = hasPrivate ? [...publicInventory, ...listEntries(registries.private, 'private')] : undefined
  const registry = await importRegistry(privateBuild ? registries.private : registries.public)
  const selected = deployment ?? registry.changelogDeployment
  const audience = typeof selected === 'string' && Object.hasOwn(changelogAudienceByDeployment, selected) ? changelogAudienceByDeployment[selected] : undefined
  if (audience === undefined || (audience === 'private') !== privateBuild)
    throw new Error(`Changelog deployment ${String(selected)} does not match a ${privateBuild ? 'private' : 'public'} build`)
  const inventory = privateBuild ? privateInventory : publicInventory
  const sources = registry.changelogSources
  const entries = compileChangelogEntries({ sources, audience, inventory })
  const privateEntries = privateBuild || privateInventory === undefined ? [] : compileChangelogEntries({
    sources: (await importRegistry(registries.private)).changelogSources,
    audience: 'private',
    inventory: privateInventory,
  }).filter(({ visibility }) => visibility === 'private')
  return {
    deployment: selected,
    entryIds: entries.map(({ id }) => id),
    privateEntries,
    privateDirectory: hasPrivate ? dirname(registries.private).replaceAll('\\', '/') : undefined,
    compile: (build) => compileChangelog({ build, sources, audience, inventory }),
  }
}

/** The exact identity a bundle embeds: evaluate the module `virtual:build-identity` resolved to. */
export const evaluateBuildIdentityModule = async (code) => {
  // Data URLs only resolve absolute URLs; the shared plugin imports its formatter by file path.
  const source = code.replace(/(\bfrom\s*|\bimport\s*\(?\s*)(["'])(\/[^"']+)\2/gu,
    (_match, head, quote, path) => `${head}${quote}${pathToFileURL(path).href}${quote}`)
  const { buildIdentity } = await nativeImport(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`)
  if (typeof buildIdentity?.machineVersion !== 'string' || buildIdentity.machineVersion === '')
    throw new Error('virtual:build-identity did not provide a machineVersion')
  return buildIdentity
}

/**
 * Privacy gate for public output, independent of any UI filter: no private manifest entry, no module
 * from the private changelog directory, and no private entry id, title, summary or link in any
 * emitted chunk or asset.
 */
export const assertPublicChangelogBoundary = ({ manifest, privateEntries, privateDirectory, modules, texts }) => {
  const privateInManifest = manifest?.entries.filter(({ visibility }) => visibility !== 'public') ?? []
  if (privateInManifest.length > 0)
    throw new Error(`${leak} entries into its manifest: ${privateInManifest.map(({ id }) => id).join(', ')}`)
  const privateModules = privateDirectory === undefined ? []
    : modules.filter((id) => id.replaceAll('\\', '/').startsWith(`${privateDirectory}/`))
  if (privateModules.length > 0) throw new Error(`${leak} modules: ${privateModules.join(', ')}`)
  const body = texts.join('\n')
  for (const entry of privateEntries)
    for (const marker of [entry.id, entry.title, entry.summary, entry.publicPrUrl])
      if (marker !== undefined && body.includes(marker)) throw new Error(`${leak} entry ${entry.id} into its output`)
}

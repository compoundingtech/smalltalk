import { fileURLToPath } from 'node:url'
import { assertBundleBoundary } from '../src/extensions/bundle-policy.mjs'
import { assertPublicChangelogBoundary, ensureChangelogRegistries, evaluateBuildIdentityModule, prepareChangelogBuild } from './changelog-build.mjs'

const changelogManifestModule = fileURLToPath(new URL('../src/changelog/build.ts', import.meta.url))
const changelogMetadataModule = fileURLToPath(new URL('../src/changelog/metadata.ts', import.meta.url))

/**
 * A fixed file replacement; there is no runtime loader, remote URL or environment-selected code.
 * Builds also replace the changelog modules with literals compiled from source files before
 * bundling; `changelogDeployment` selects a channel (e.g. `stable`) other than the registry default.
 */
export const extensionBuild = ({ entry, privateBuild, changelogDeployment, changelogRegistries }) => {
  let building = false
  let changelog
  let manifest
  let identityId
  const compile = async (identityCode) => {
    const { machineVersion } = await evaluateBuildIdentityModule(identityCode)
    manifest = changelog.compile({ machineVersion })
  }
  return {
    name: 'fractal:build-time-extensions',
    enforce: 'pre',
    configResolved(config) {
      building = config.command === 'build'
      // The dev server imports the registry from the browser graph without a build start.
      ensureChangelogRegistries(changelogRegistries)
    },
    // Compile independently of whether the UI graph reaches the changelog modules (production
    // excludes the devbar). The identity is the code this build bundles for `virtual:build-identity`,
    // observed when that module is transformed, so the manifest and the bundle cannot disagree.
    async buildStart() {
      manifest = undefined
      changelog = undefined
      identityId = undefined
      if (!building) return
      changelog = await prepareChangelogBuild({
        privateBuild,
        deployment: changelogDeployment,
        ...(changelogRegistries === undefined ? {} : { registries: changelogRegistries }),
      })
      const identity = await this.resolve('virtual:build-identity')
      if (identity === null) throw new Error('virtual:build-identity is not resolvable in this build')
      identityId = identity.id
    },
    resolveId(source, importer) {
      if (source.endsWith('/extensions/build.ts') ||
        (source === './build.ts' && importer?.includes('/src/extensions/')))
        return new URL(entry, import.meta.url).pathname
    },
    async transform(code, id) {
      if (changelog !== undefined && id === identityId) await compile(code)
    },
    async load(id) {
      if (changelog === undefined) return
      const path = id.split('?')[0]
      if (path === changelogMetadataModule)
        return `export const changelogEntryIds = Object.freeze(${JSON.stringify(changelog.entryIds)});\nexport const changelogDeployment = ${JSON.stringify(changelog.deployment)};\n`
      if (path !== changelogManifestModule) return
      if (manifest === undefined) {
        // Loading the identity module runs the transform above on the same canonical code.
        const info = await this.load({ id: identityId })
        if (manifest === undefined && typeof info?.code === 'string') await compile(info.code)
        if (manifest === undefined) throw new Error('virtual:build-identity produced no code to compile the changelog manifest against')
      }
      return `export const changelogManifest = ${JSON.stringify(manifest)};\nexport const changelogDeployment = ${JSON.stringify(changelog.deployment)};\n`
    },
    generateBundle(_options, bundle) {
      const modules = [...this.getModuleIds()]
      const texts = Object.values(bundle).map((item) =>
        item.type === 'chunk' ? item.code : typeof item.source === 'string'
          ? item.source : new TextDecoder().decode(item.source))
      assertBundleBoundary({ modules, texts, privateBuild })
      if (changelog === undefined) return
      if (manifest === undefined) throw new Error('The build never transformed virtual:build-identity; no changelog manifest was compiled for it')
      if (!privateBuild)
        assertPublicChangelogBoundary({ manifest, privateEntries: changelog.privateEntries, privateDirectory: changelog.privateDirectory, modules, texts })
      this.emitFile({ type: 'asset', fileName: 'changelog.v1.json', source: `${JSON.stringify(manifest, null, 2)}\n` })
    },
  }
}

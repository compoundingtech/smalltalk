import { createGenieOutput } from '../../repos/effect-utils/packages/@overeng/genie/src/runtime/core.ts'
import {
  renderPnpmPackageTargets,
  renderPnpmPlatformGatedPackages,
  renderPnpmStoreBuck,
} from '../../repos/effect-utils/buck2/dependencies/pnpm-store-buck.ts'
import { makePnpmStoreProjection } from '../../repos/effect-utils/buck2/dependencies/pnpm-store.ts'
import { loadBuckLockData } from './lock.ts'

const { metadata, sidecar } = await loadBuckLockData()
const store = makePnpmStoreProjection({ metadata, sidecar })

// The renderer targets the rules' own dependency package; this root loads the same rules and
// store runtime from the `rules` cell instead.
const producerPreamble = `load(":defs.bzl", "pnpm_platform_configurations", "pnpm_store_entry", "pnpm_store_scc", "pnpm_store_view")

pnpm_platform_configurations()

export_file(
    name = "assemble-store.ts",
    src = "assemble-store.ts",
    visibility = ["PUBLIC"],
)
`
const consumerPreamble = `load("@rules//buck2/dependencies:defs.bzl", "pnpm_platform_configurations", "pnpm_store_entry", "pnpm_store_scc", "pnpm_store_view")

pnpm_platform_configurations()

alias(
    name = "assemble-store.ts",
    actual = "@rules//buck2/dependencies:assemble-store.ts",
    visibility = ["PUBLIC"],
)
`
const storeBuck = renderPnpmStoreBuck(store)
if (storeBuck.startsWith(producerPreamble) === false) {
  throw new Error('the effect-utils store renderer changed its preamble; update buck2/dependencies/BUCK.genie.ts')
}

export default createGenieOutput({
  data: { store },
  stringify: () => `# Projected from pnpm-lock.yaml for the Buck importers in lock.ts.
# Store fingerprint: ${store.fingerprint}

load("@rules//buck2/dependencies:defs.bzl", "pnpm_package", "pnpm_platform_gated_packages")

${renderPnpmPlatformGatedPackages({ metadata })}
${renderPnpmPackageTargets({ metadata, sidecar })}
${consumerPreamble}${storeBuck.slice(producerPreamble.length)}`,
})

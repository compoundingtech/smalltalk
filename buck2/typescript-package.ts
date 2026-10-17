import { readdirSync } from 'node:fs'

import { pnpmTargetName } from '../repos/effect-utils/buck2/dependencies/pnpm-lock.ts'
import { createGenieOutput } from '../repos/effect-utils/packages/@overeng/genie/src/runtime/core.ts'
import { buckImporters } from './dependencies/lock.ts'

const quote = (value: string) => JSON.stringify(value)

/**
 * One flat TypeScript workspace package (sources beside its package.json): ordinary and product
 * source trees over their corresponding lock-derived dependency views, then one `tsgo_typecheck`
 * per project file over the ordinary tree. Both trees declare the same package-local inputs.
 */
export const typescriptPackageBuck = ({
  packagePath,
  projects,
}: {
  packagePath: (typeof buckImporters)[number]
  projects: Readonly<Record<string, string>>
}) => {
  const files = readdirSync(new URL(`../${packagePath}/`, import.meta.url), { withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => entry.name)
    .filter(
      (name) =>
        name === 'package.json' ||
        (name.endsWith('.ts') && name.endsWith('.genie.ts') === false) ||
        /^tsconfig.*\.json$/.test(name),
    )
    // Byte order, independent of the filesystem's directory order.
    .toSorted((left, right) => (left < right ? -1 : left > right ? 1 : 0))
  for (const project of Object.values(projects)) {
    if (files.includes(project) === false) throw new Error(`${packagePath} has no ${project}`)
  }
  const trees = [
    ['package_tree', 'view'],
    ['product_package_tree', 'product_view'],
  ] as const
  const text = [
    'load("@rules//buck2:materialization.bzl", "package_view")',
    'load("@rules//buck2:typescript.bzl", "tsgo_typecheck")',
    '',
    ...trees.flatMap(([name, prefix]) => [
      'package_view(',
      `    name = ${quote(name)},`,
      `    dependency_view = ${quote(`//buck2/dependencies:${pnpmTargetName({ prefix, identity: packagePath })}`)},`,
      '    files = {',
      ...files.map((file) => `        ${quote(file)}: ${quote(file)},`),
      '    },',
      '    strip_project_references = True,',
      '    runtime = "//:package_tree_runtime",',
      '    runtime_entry = "package-tree.ts",',
      '    visibility = ["PUBLIC"],',
      ')',
      '',
    ]),
    ...Object.entries(projects).flatMap(([name, project]) => [
      '',
      'tsgo_typecheck(',
      `    name = ${quote(name)},`,
      '    package_tree = ":package_tree",',
      `    project = ${quote(project)},`,
      '    visibility = ["PUBLIC"],',
      ')',
    ]),
    '',
  ].join('\n')
  return createGenieOutput({ data: { packagePath, files, projects }, stringify: () => text })
}

/** Every typecheck target in the Buck graph, for the root aggregate and CI. */
export const typecheckTargets = {
  'clients/typescript/st3-client': { typecheck: 'tsconfig.json', typecheck_schema: 'tsconfig.schema.json' },
  'clients/typescript/st3-views': { typecheck: 'tsconfig.json' },
} as const satisfies Record<(typeof buckImporters)[number], Record<string, string>>

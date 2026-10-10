import { buckRootFile, rootBuck } from './buck2/root.ts'
import { typecheckTargets } from './buck2/typescript-package.ts'

const verdicts = Object.entries(typecheckTargets).flatMap(([packagePath, projects]) =>
  Object.keys(projects).map((name) => `        "${packagePath}/${name}": "//${packagePath}:${name}",`),
)

// `buck2 build //:typecheck` checks every admitted TypeScript project.
export default buckRootFile({
  text: rootBuck,
  extra: `filegroup(
    name = "typecheck",
    srcs = {
${verdicts.join('\n')}
    },
    visibility = ["PUBLIC"],
)
`,
})
